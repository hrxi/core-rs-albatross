use std::collections::{hash_map, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::num::NonZeroU8;
use std::{fs, io, process};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use clap::Parser;
use futures::future::BoxFuture;
use futures::StreamExt;
use log::{error, info};
use log::level_filters::LevelFilter;
use nimiq::{
    error::Error,
    extras::{
        logging::{initialize_logging, log_error_cause_chain},
        panic::initialize_panic_reporting,
        signal_handling::initialize_signal_handler,
    },
};
use nimiq::config::config_file::LogSettings;
use nimiq_block::{Block, MacroBlock, MacroHeader};
use nimiq_consensus::messages::{BlockBodyTopic, BlockHeaderTopic, RequestBlock, RequestHead, ResponseHead};
use nimiq_genesis::NetworkInfo;
use nimiq_hash::Blake2bHash;
use nimiq_hash::nimiq_serde::Serialize;
use nimiq_network_libp2p::{Keypair, Network};
use nimiq_primitives::{networks::NetworkId, policy::Policy};
use nimiq_network_libp2p::{Config as NetworkConfig, PeerId};
use nimiq_network_interface::Multiaddr;
use nimiq_network_interface::network::{MsgAcceptance, Network as _, PubsubId};
use nimiq_network_interface::peer_info::Services;
use nimiq_network_interface::request::Request;
use nimiq_network_libp2p::discovery::peer_contacts::PeerContact;
use rand::{rngs::StdRng, seq::SliceRandom as _, SeedableRng as _};
use tokio::time::{interval, sleep, MissedTickBehavior};

#[derive(Debug, Parser)]
pub struct Args {
    #[clap(long)]
    pub log_level: Option<LevelFilter>,

    pub network: NetworkId,

    #[clap(required = true)]
    pub seed: Vec<Multiaddr>,
}

fn handle_requests<T: Request, F>(network: Arc<Network>, mut f: F) -> BoxFuture<'static, ()> where
    F: FnMut(T) -> T::Response + Send + 'static
{
    let mut requests = network.receive_requests::<T>();
    Box::pin(async move {
        while let Some((request, id, _)) = requests.next().await {
            network.respond::<T>(id, f(request)).await.unwrap();
        }
    })
}

fn write_atomically(filename: &str, contents: &[u8]) -> io::Result<()> {
    let mut tmp_filename = Path::new(filename).as_os_str().to_owned();
    tmp_filename.push(format!(".tmp.{}", process::id()));
    let tmp_filename = PathBuf::from(tmp_filename);
    match fs::write(&tmp_filename, contents) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = Path::new(&tmp_filename).parent() {
                fs::create_dir_all(parent)?;
                fs::write(&tmp_filename, contents)?;
            } else {
                return Err(e);
            }
        }
        result => result?,
    }
    fs::rename(&tmp_filename, filename)?;
    Ok(())
}

fn symlink_dir<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<()> {
    #[cfg(windows)]
    fn symlink_dir_os(original: &Path, link: &Path) -> io::Result<()> {
        use std::os::windows::fs;
        fs::symlink_dir(original, link)
    }
    #[cfg(unix)]
    fn symlink_dir_os(original: &Path, link: &Path) -> io::Result<()> {
        use std::os::unix::fs;
        fs::symlink(original, link)
    }

    fn symlink_dir_impl(original: &Path, link: &Path) -> io::Result<()> {
        match symlink_dir_os(original, link) {
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            // io::ErrorKind::NotADirectory isn't stable yet:
            // https://github.com/rust-lang/rust/issues/86442
            Err(err) /*if err.kind() == io::ErrorKind::NotADirectory*/ => {
                if let Some(parent) = link.parent() {
                    fs::create_dir_all(parent)?;
                    match symlink_dir_os(original, link) {
                        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                        result => result,
                    }
                } else {
                    Err(err)
                }
            }
            result => result,
        }
    }

    symlink_dir_impl(original.as_ref(), link.as_ref())
}

fn hash_to_dir(block_hash: &Blake2bHash) -> String {
    let hash = block_hash.to_string();
    format!("{}/{}/{}", &hash[..2], &hash[2..4], &hash[4..])
}

fn block_ref(block_hash: &Blake2bHash) -> String {
    format!("../../../{}", hash_to_dir(block_hash))
}

fn filename_block(block_hash: &Blake2bHash, name: &str) -> String {
    format!("blocks/{}/{}", hash_to_dir(block_hash), name)
}

fn filename_missing_block(block_hash: &Blake2bHash) -> String {
    format!("missing/{}", block_hash)
}

fn create_missing_block(block_hash: &Blake2bHash) -> io::Result<()> {
    symlink_dir(format!("../blocks/{}", hash_to_dir(block_hash)), filename_missing_block(block_hash))
}

fn read_missing_hashes() -> io::Result<Vec<Blake2bHash>> {
    let dir = match fs::read_dir("missing") {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        result => result?,
    };
    let mut result = Vec::new();
    for entry in dir {
        let filename = entry?.file_name();
        let hash = filename.to_string_lossy().parse()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        result.push(hash);
    }
    Ok(result)
}

fn write_block(block: &Block) -> io::Result<()> {
    let block_hash = block.hash();
    write_atomically(&filename_block(&block_hash, "block"), &block.serialize_to_vec())?;
    symlink_dir(block_ref(block.parent_hash()), filename_block(&block_hash, "parent"))?;
    if let Some(parent_election_hash) = block.parent_election_hash() {
        symlink_dir(block_ref(parent_election_hash), filename_block(&block_hash, "parent_election"))?;
    }
    {
        let no = block.block_number();
        symlink_dir(format!("../../../blocks/{}", hash_to_dir(&block_hash)), format!("by-height/{}/{:03}/{:03}/{}", no / 1000 / 1000, no / 1000 % 1000, no % 1000, block_hash))?;
    }
    fs::remove_file(filename_missing_block(&block_hash))?;
    Ok(())
}

fn verify_block(hash: &Blake2bHash, mut block: Block) -> Option<Block> {
    if block.hash_cached() != *hash {
        return None;
    }
    block.verify(block.network()).ok()?;
    block.body()?;
    block.justification()?;
    Some(block)
}

type ResolveBlock = Arc<dyn Fn(Blake2bHash, Arc<Mutex<BlockState>>) -> BoxFuture<'static, Option<(Block, PeerId)>> + Send + Sync>;

struct Blocks {
    resolve: ResolveBlock,
    state: Arc<Mutex<BlocksState>>,
}

#[derive(Default)]
struct BlocksState {
    pending: HashMap<Blake2bHash, Arc<Mutex<BlockState>>>,
}

#[derive(Default)]
struct BlockState {
    hint_queue: VecDeque<PeerId>,
    hint_set: HashSet<PeerId>,
}

impl BlockState {
    fn push_hint(&mut self, peer_id: &PeerId) {
        if !self.hint_set.insert(*peer_id) {
            return;
        }
        self.hint_queue.push_back(*peer_id);
    }
    fn pop_hint(&mut self) -> Option<PeerId> {
        let Some(peer_id) = self.hint_queue.pop_front() else { return None; };
        assert!(self.hint_set.remove(&peer_id));
        Some(peer_id)
    }
}

impl Blocks {
    fn new(resolve: ResolveBlock) -> Blocks {
        Blocks {
            resolve,
            state: Default::default(),
        }
    }
    fn duplicate(&self) -> Blocks {
        Blocks {
            resolve: Arc::clone(&self.resolve),
            state: Arc::clone(&self.state),
        }
    }
    fn observe_block_header(&self, block: &Block, peer_id: Option<PeerId>) -> bool {
        let mut result = false;
        result |= self.observe(&block.hash(), peer_id);
        result |= self.observe(block.parent_hash(), peer_id);
        if let Some(parent_election_hash) = block.parent_election_hash() {
            result |= self.observe(parent_election_hash, peer_id);
        }
        if let Block::Macro(MacroBlock { header: MacroHeader { interlink: Some(interlinks), .. }, ..}) = block {
            for interlink in interlinks {
                result |= self.observe(interlink, peer_id);
            }
        }
        result
    }
    fn observe(&self, hash: &Blake2bHash, peer_id: Option<PeerId>) -> bool {
        let push_hint = move |block_state: &Mutex<BlockState>| {
            if let Some(peer_id) = peer_id {
                block_state.lock().unwrap().push_hint(&peer_id);
            }
        };
        let mut state = self.state.lock().unwrap();
        let vacant = match state.pending.entry(hash.clone()) {
            hash_map::Entry::Occupied(o) => {
                push_hint(o.get());
                return false;
            }
            hash_map::Entry::Vacant(v) => v,
        };
        if Path::new(&filename_block(&hash, "block")).try_exists().unwrap() {
            return false;
        }
        info!("getting {}", &hash.to_string()[..16]);
        create_missing_block(hash).unwrap();
        let block_state = Arc::clone(vacant.insert(Default::default()));
        push_hint(&block_state);
        tokio::spawn({
            let this = self.duplicate();
            let hash = hash.clone();
            let state = Arc::clone(&self.state);
            let resolve = (self.resolve)(hash.clone(), block_state);
            async move {
                let write_result = if let Some((block, peer_id)) = resolve.await {
                    info!("writing {} ({})", &hash.to_string()[..16], block.block_number());
                    let result = write_block(&block);
                    if this.observe_block_header(&block, Some(peer_id)) {
                        info!("(block {})", &hash.to_string()[..16]);
                    }
                    result
                } else {
                    error!("couldn't get {}", &hash.to_string()[..16]);
                    Ok(())
                };
                state.lock().unwrap().pending.remove(&hash);
                write_result.unwrap();
            }
        });
        true
    }
}

async fn main_inner() -> Result<(), Error> {
    let args = Args::parse();

    let log_settings = LogSettings {
        level: args.log_level,
        timestamps: true,
        tags: Default::default(),
        statistics: LogSettings::default_statistics_interval(),
        file: None,
        loki: None,
        tokio_console_bind_address: None,
    };

    initialize_logging(None, Some(&log_settings))?;
    initialize_panic_reporting();
    initialize_signal_handler();

    // Get network info (i.e. which specific blockchain we're on)
    if !args.network.is_albatross() {
        return Err(Error::config_error(format!(
            "{} is not compatible with Albatross",
            args.network,
        )));
    }
    let network_info = NetworkInfo::from_network_id(args.network);
    let genesis_block_number = network_info.genesis_block().block_number();
    let genesis_block_hash = network_info.genesis_hash().clone();

    let policy_config = Policy {
        genesis_block_number,
        ..Default::default()
    };

    let _ = Policy::get_or_init(policy_config);

    // Verify Policy is configured with the genesis block number we expect
    if genesis_block_number != Policy::genesis_block_number() {
        log::error!("The genesis block number must be configured before using any other Policy function");
        return Err(Error::config_error(
            "There is a genesis block number configuration mismatch",
        ));
    }

    let identity_keypair = Keypair::generate_ed25519();

    let provided_services = Services::FULL_BLOCKS;
    let required_services = Services::FULL_BLOCKS;

    let peer_contact = PeerContact::new(
        [],
        identity_keypair.public(),
        provided_services,
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
        .map_err(|e| Error::Network(nimiq_network_libp2p::NetworkError::PeerContactError(e)))?;

    let network_config = NetworkConfig::new(
        identity_keypair,
        peer_contact,
        args.seed,
        genesis_block_hash.clone(),
        false,
        required_services,
        None,
        12, // the default
        4000, // the default
        4, // lower
        20, // lower
        false,
        false,
        NonZeroU8::new(3).unwrap(),
    );

    let network = Arc::new(Network::new(network_config).await);

    network.listen_on(["/ip4/0.0.0.0/tcp/8444/ws", "/ip6/::/tcp/8444/ws"].into_iter().map(|s| s.parse().unwrap()).collect()).await;

    tokio::spawn(handle_requests(Arc::clone(&network), move |RequestHead {}: RequestHead| {
        ResponseHead {
            block_number: genesis_block_number,
            block_hash: genesis_block_hash.clone(),
            macro_hash: genesis_block_hash.clone(),
            election_hash: genesis_block_hash.clone(),
        }
    }));

    let resolve_block = {
        let network = Arc::clone(&network);
        move |hash: Blake2bHash, block_state: Arc<Mutex<BlockState>>| {
            let network = Arc::clone(&network);
            Box::pin(async move {
                let mut peers_tried = HashSet::new();
                loop {
                    let peer_id = {
                        let mut block_state = block_state.lock().unwrap();
                        loop {
                            match block_state.pop_hint() {
                                Some(peer_id) => {
                                    if peers_tried.insert(peer_id) {
                                        break Some(peer_id);
                                    }
                                }
                                None => break None,
                            }
                        }
                    };
                    let peer_id = match peer_id {
                        Some(peer_id) => peer_id,
                        None => {
                            let peer_id = match network.get_peers_by_services(Services::FULL_BLOCKS, 1).await {
                                Ok(peers) => {
                                    peers.into_iter().filter(|peer_id| !peers_tried.contains(peer_id)).next()
                                }
                                Err(_) => None,
                            };
                            match peer_id {
                                Some(peer_id) => peer_id,
                                None => {
                                    if peers_tried.is_empty() {
                                        // TODO: better retry logic
                                        sleep(Duration::from_secs(10)).await;
                                        continue;
                                    }
                                    error!("couldn't find peer for {} after requesting from {} peers", &hash.to_string()[..16], peers_tried.len());
                                    return None;
                                }
                            }
                        }
                    };
                    peers_tried.insert(peer_id);
                    let block = match network.request(RequestBlock {
                        hash: hash.clone(),
                        include_body: true,
                    }, peer_id).await {
                        Ok(Ok(response)) => verify_block(&hash, response),
                        Ok(Err(err)) => {
                            error!("error response {} from {}: {}", &hash.to_string()[..16], peer_id, err);
                            None
                        }
                        Err(err) => {
                            error!("couldn't request {} from {}: {}", &hash.to_string()[..16], peer_id, err);
                            None
                        }
                    };
                    if let Some(block) = block {
                        return Some((block, peer_id));
                    }
                }
            }) as BoxFuture<'static, Option<(Block, PeerId)>>
        }
    };
    let blocks = Blocks::new(Arc::new(resolve_block) as ResolveBlock);

    network.start_connecting().await;

    {
        let missing_hashes = read_missing_hashes().unwrap();
        for missing in &missing_hashes {
            blocks.observe(missing, None);
        }
        if !missing_hashes.is_empty() {
            info!("({} missing blocks from a previous run)", missing_hashes.len());
        }
    }

    tokio::spawn({
        let network = Arc::clone(&network);
        let blocks = blocks.duplicate();
        let mut interval = interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut rng = StdRng::from_entropy();
        async move {
            let mut first = true;
            loop {
                if first {
                    first = false;
                } else {
                    interval.tick().await;
                }
                let Ok(peers) = network.get_peers_by_services(Services::FULL_BLOCKS, 1).await else {
                    continue;
                };
                let Some(&peer_id) = peers.choose(&mut rng) else { continue; };
                let Ok(head) = network.request(RequestHead {}, peer_id).await else { continue; };
                if blocks.observe(&head.block_hash, Some(peer_id)) |
                    blocks.observe(&head.macro_hash, Some(peer_id)) |
                    blocks.observe(&head.election_hash, Some(peer_id))
                {
                    info!("(head from {})", peer_id);
                }
            }
        }
    });

    tokio::spawn({
        let mut block_headers = network.subscribe::<BlockHeaderTopic>().await.unwrap();
        let network = Arc::clone(&network);
        let blocks = blocks.duplicate();
        async move {
            while let Some((msg, pubsub_id)) = block_headers.next().await {
                let mut block: Block = msg.into();
                block.hash_cached();
                let peer_id = pubsub_id.propagation_source();
                network.validate_message::<BlockHeaderTopic>(pubsub_id, MsgAcceptance::Accept);
                if blocks.observe_block_header(&block, Some(peer_id)) {
                    info!("(block header from {})", peer_id);
                }
            }
        }
    }).await.unwrap();
    /*
    tokio::spawn({
        let mut block_bodies = network.subscribe::<BlockBodyTopic>().await.unwrap();
        let network = Arc::clone(&network);
        async move {
            while let Some((msg, pubsub_id)) = block_bodies.next().await {
                let peer_id = pubsub_id.propagation_source();
                network.validate_message::<BlockBodyTopic>(pubsub_id, MsgAcceptance::Accept);
                error!("body {} {}", &msg.body.hash().to_string()[..16], peer_id);
            }
        }
    }).await;
    */
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = main_inner().await {
        log_error_cause_chain(&e);
    }
}
