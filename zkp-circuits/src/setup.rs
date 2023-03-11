use std::fs::{DirBuilder, File};
use std::path::Path;

use ark_crypto_primitives::snark::CircuitSpecificSetupSNARK;
use ark_ec::{mnt6::MNT6, pairing::Pairing, CurveGroup};
use ark_groth16::{Groth16, Proof, ProvingKey, VerifyingKey};
use ark_mnt4_753::{G1Projective as G1MNT4, G2Projective as G2MNT4, MNT4_753};
use ark_mnt6_753::{Config, G1Projective as G1MNT6, G2Projective as G2MNT6, MNT6_753};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use ark_std::UniformRand;
use rand::{CryptoRng, Rng};

use nimiq_primitives::policy::Policy;
use nimiq_zkp_primitives::{MacroBlock, NanoZKPError, PK_TREE_BREADTH};

use crate::{
    circuits::mnt4::{
        MacroBlockWrapperCircuit, MergerWrapperCircuit, PKTreeNodeCircuit as NodeMNT4,
    },
    circuits::mnt6::{
        MacroBlockCircuit, MergerCircuit, PKTreeLeafCircuit as LeafMNT6,
        PKTreeNodeCircuit as NodeMNT6,
    },
};

pub const DEVELOPMENT_SEED: [u8; 32] = [
    1, 0, 52, 0, 0, 0, 0, 0, 1, 0, 10, 0, 22, 32, 0, 0, 2, 0, 55, 49, 0, 11, 0, 0, 3, 0, 0, 0, 0,
    0, 2, 92,
];

/// This function generates the parameters (proving and verifying keys) for the entire light macro sync.
/// It does this by generating the parameters for each circuit, "from bottom to top". The
/// order is absolutely necessary because each circuit needs a verifying key from the circuit "below"
/// it. Note that the parameter generation can take longer than one hour, even two on some computers.
pub fn setup<R: Rng + CryptoRng>(
    mut rng: R,
    path: &Path,
    prover_active: bool,
) -> Result<(), NanoZKPError> {
    if all_files_created(path, prover_active) {
        return Ok(());
    }

    setup_pk_tree_leaf(&mut rng, path, "pk_tree_5")?;

    setup_pk_tree_node_mnt4(&mut rng, path, "pk_tree_5", "pk_tree_4", 4)?;

    setup_pk_tree_node_mnt6(&mut rng, path, "pk_tree_4", "pk_tree_3", 3)?;

    setup_pk_tree_node_mnt4(&mut rng, path, "pk_tree_3", "pk_tree_2", 2)?;

    setup_pk_tree_node_mnt6(&mut rng, path, "pk_tree_2", "pk_tree_1", 1)?;

    setup_pk_tree_node_mnt4(&mut rng, path, "pk_tree_1", "pk_tree_0", 0)?;

    setup_macro_block(&mut rng, path)?;

    setup_macro_block_wrapper(&mut rng, path)?;

    setup_merger(&mut rng, path)?;

    setup_merger_wrapper(&mut rng, path)?;

    Ok(())
}

pub fn load_verifying_key_from_file(
    path: &Path,
) -> Result<VerifyingKey<MNT6<Config>>, NanoZKPError> {
    // Loads the verifying key from the preexisting file.
    // Note: We only use the merger_wrapper for key verification purposes.
    let mut file = File::open(path.join("verifying_keys").join("merger_wrapper.bin"))?;
    let vk: VerifyingKey<MNT6<Config>> =
        VerifyingKey::deserialize_uncompressed_unchecked(&mut file)?;

    Ok(vk)
}

pub fn all_files_created(path: &Path, prover_active: bool) -> bool {
    let verifying_keys = path.join("verifying_keys");
    let proving_keys = path.join("proving_keys");

    if prover_active {
        for i in 0..5 {
            if !verifying_keys.join(format!("pk_tree_{i}.bin")).exists()
                || !proving_keys.join(format!("pk_tree_{i}.bin")).exists()
            {
                return false;
            }
        }
    }

    verifying_keys.join("merger_wrapper.bin").exists()
        && (!prover_active
            || (verifying_keys.join("macro_block.bin").exists()
                && verifying_keys.join("macro_block_wrapper.bin").exists()
                && verifying_keys.join("merger.bin").exists()
                && proving_keys.join("merger_wrapper.bin").exists()
                && proving_keys.join("macro_block_wrapper.bin").exists()
                && proving_keys.join("merger.bin").exists()
                && proving_keys.join("merger_wrapper.bin").exists()))
}

fn setup_pk_tree_leaf<R: Rng + CryptoRng>(
    rng: &mut R,
    dir_path: &Path,
    name: &str,
) -> Result<(), NanoZKPError> {
    // Create dummy inputs.
    let pks = vec![G2MNT6::rand(rng); Policy::SLOTS as usize / PK_TREE_BREADTH];

    let mut pk_tree_root = [0u8; 95];
    rng.fill_bytes(&mut pk_tree_root);

    let mut agg_pk_commitment = [0u8; 95];
    rng.fill_bytes(&mut agg_pk_commitment);

    let mut signer_bitmap = Vec::with_capacity(Policy::SLOTS as usize);
    for _ in 0..Policy::SLOTS {
        signer_bitmap.push(rng.gen());
    }

    // Create parameters for our circuit
    let circuit = LeafMNT6::new(pks, pk_tree_root, agg_pk_commitment, signer_bitmap);

    let (pk, vk) = Groth16::<MNT4_753>::setup(circuit, rng)?;

    // Save keys to file.
    keys_to_file(&pk, &vk, name, dir_path)
}

fn setup_pk_tree_node_mnt4<R: Rng + CryptoRng>(
    rng: &mut R,
    dir_path: &Path,
    vk_file: &str,
    name: &str,
    tree_level: usize,
) -> Result<(), NanoZKPError> {
    // Load the verifying key from file.
    let mut file = File::open(
        dir_path
            .join("verifying_keys")
            .join(format!("{vk_file}.bin")),
    )?;

    let vk_child = VerifyingKey::deserialize_uncompressed_unchecked(&mut file)?;

    // Create dummy inputs.
    let left_proof = Proof {
        a: G1MNT4::rand(rng).into_affine(),
        b: G2MNT4::rand(rng).into_affine(),
        c: G1MNT4::rand(rng).into_affine(),
    };

    let right_proof = Proof {
        a: G1MNT4::rand(rng).into_affine(),
        b: G2MNT4::rand(rng).into_affine(),
        c: G1MNT4::rand(rng).into_affine(),
    };

    let mut pk_node_hash = [0u8; 95];
    rng.fill_bytes(&mut pk_node_hash);

    let mut l_pk_node_hash = [0u8; 95];
    rng.fill_bytes(&mut l_pk_node_hash);
    let mut r_pk_node_hash = [0u8; 95];
    rng.fill_bytes(&mut r_pk_node_hash);

    let mut left_agg_pk_commitment = [0u8; 95];
    rng.fill_bytes(&mut left_agg_pk_commitment);

    let mut right_agg_pk_commitment = [0u8; 95];
    rng.fill_bytes(&mut right_agg_pk_commitment);

    let mut signer_bitmap = Vec::with_capacity(Policy::SLOTS as usize);
    for _ in 0..Policy::SLOTS {
        signer_bitmap.push(rng.gen());
    }

    // Create parameters for our circuit
    let circuit = NodeMNT4::new(
        tree_level,
        vk_child,
        left_proof,
        right_proof,
        l_pk_node_hash,
        r_pk_node_hash,
        left_agg_pk_commitment,
        right_agg_pk_commitment,
        signer_bitmap,
    );

    let (pk, vk) = Groth16::<MNT6_753>::setup(circuit, rng)?;

    // Save keys to file.
    keys_to_file(&pk, &vk, name, dir_path)
}

fn setup_pk_tree_node_mnt6<R: Rng + CryptoRng>(
    rng: &mut R,
    dir_path: &Path,
    vk_file: &str,
    name: &str,
    tree_level: usize,
) -> Result<(), NanoZKPError> {
    // Load the verifying key from file.
    let mut file = File::open(
        dir_path
            .join("verifying_keys")
            .join(format!("{vk_file}.bin")),
    )?;

    let vk_child = VerifyingKey::deserialize_uncompressed_unchecked(&mut file)?;

    // Create dummy inputs.
    let left_proof = Proof {
        a: G1MNT6::rand(rng).into_affine(),
        b: G2MNT6::rand(rng).into_affine(),
        c: G1MNT6::rand(rng).into_affine(),
    };

    let right_proof = Proof {
        a: G1MNT6::rand(rng).into_affine(),
        b: G2MNT6::rand(rng).into_affine(),
        c: G1MNT6::rand(rng).into_affine(),
    };

    let agg_pks = vec![G2MNT6::rand(rng); 4];

    let mut pk_node_hashes = vec![];
    for _ in 0..4 {
        let mut pk_node_hash = [0u8; 95];
        rng.fill_bytes(&mut pk_node_hash);
        pk_node_hashes.push(pk_node_hash);
    }
    let mut pk_node_hash = [0u8; 95];
    rng.fill_bytes(&mut pk_node_hash);

    let mut agg_pk_commitment = [0u8; 95];
    rng.fill_bytes(&mut agg_pk_commitment);

    let mut signer_bitmap = Vec::with_capacity(Policy::SLOTS as usize);
    for _ in 0..Policy::SLOTS {
        signer_bitmap.push(rng.gen());
    }

    // Create parameters for our circuit
    let circuit = NodeMNT6::new(
        tree_level,
        vk_child,
        left_proof,
        right_proof,
        agg_pks[0],
        agg_pks[1],
        agg_pks[2],
        agg_pks[3],
        pk_node_hashes[0],
        pk_node_hashes[1],
        pk_node_hashes[2],
        pk_node_hashes[3],
        pk_node_hash,
        agg_pk_commitment,
        signer_bitmap,
    );

    let (pk, vk) = Groth16::<MNT4_753>::setup(circuit, rng)?;

    // Save keys to file.
    keys_to_file(&pk, &vk, name, dir_path)
}

fn setup_macro_block<R: Rng + CryptoRng>(rng: &mut R, path: &Path) -> Result<(), NanoZKPError> {
    // Load the verifying key from file.
    let mut file = File::open(path.join("verifying_keys").join("pk_tree_0.bin"))?;

    let vk_pk_tree = VerifyingKey::deserialize_uncompressed_unchecked(&mut file)?;

    // Create dummy inputs.
    let proof = Proof {
        a: G1MNT6::rand(rng).into_affine(),
        b: G2MNT6::rand(rng).into_affine(),
        c: G1MNT6::rand(rng).into_affine(),
    };

    let mut initial_pk_tree_root = [0u8; 95];
    rng.fill_bytes(&mut initial_pk_tree_root);

    let mut initial_header_hash = [0u8; 32];
    rng.fill_bytes(&mut initial_header_hash);

    let block_number = u32::rand(rng);

    let round_number = u32::rand(rng);

    let mut final_pk_tree_root = [0u8; 95];
    rng.fill_bytes(&mut final_pk_tree_root);

    let mut initial_state_commitment = [0u8; 95];
    rng.fill_bytes(&mut initial_state_commitment);

    let mut final_state_commitment = [0u8; 95];
    rng.fill_bytes(&mut final_state_commitment);

    let mut header_hash = [0u8; 32];
    rng.fill_bytes(&mut header_hash);

    let signature = G1MNT6::rand(rng);

    let mut signer_bitmap = Vec::with_capacity(Policy::SLOTS as usize);
    for _ in 0..Policy::SLOTS {
        signer_bitmap.push(rng.gen());
    }

    let block = MacroBlock {
        block_number,
        round_number,
        header_hash,
        signature,
        signer_bitmap,
    };

    let mut l_pk_node_hash = [0u8; 95];
    rng.fill_bytes(&mut l_pk_node_hash);

    let mut r_pk_node_hash = [0u8; 95];
    rng.fill_bytes(&mut r_pk_node_hash);

    let l_agg_commitment = G2MNT6::rand(rng);

    let r_agg_commitment = G2MNT6::rand(rng);

    // Create parameters for our circuit
    let circuit = MacroBlockCircuit::new(
        vk_pk_tree,
        proof,
        initial_pk_tree_root,
        initial_header_hash,
        final_pk_tree_root,
        block,
        l_pk_node_hash,
        r_pk_node_hash,
        l_agg_commitment,
        r_agg_commitment,
        initial_state_commitment,
        final_state_commitment,
    );

    let (pk, vk) = Groth16::<MNT4_753>::setup(circuit, rng)?;

    // Save keys to file.
    keys_to_file(&pk, &vk, "macro_block", path)
}

fn setup_macro_block_wrapper<R: Rng + CryptoRng>(
    rng: &mut R,
    path: &Path,
) -> Result<(), NanoZKPError> {
    // Load the verifying key from file.
    let mut file = File::open(path.join("verifying_keys").join("macro_block.bin"))?;

    let vk_macro_block = VerifyingKey::deserialize_uncompressed_unchecked(&mut file)?;

    // Create dummy inputs.
    let proof = Proof {
        a: G1MNT4::rand(rng).into_affine(),
        b: G2MNT4::rand(rng).into_affine(),
        c: G1MNT4::rand(rng).into_affine(),
    };

    let mut initial_state_commitment = [0u8; 95];
    rng.fill_bytes(&mut initial_state_commitment);

    let mut final_state_commitment = [0u8; 95];
    rng.fill_bytes(&mut final_state_commitment);

    // Create parameters for our circuit
    let circuit = MacroBlockWrapperCircuit::new(
        vk_macro_block,
        proof,
        initial_state_commitment,
        final_state_commitment,
    );

    let (pk, vk) = Groth16::<MNT6_753>::setup(circuit, rng)?;

    // Save keys to file.
    keys_to_file(&pk, &vk, "macro_block_wrapper", path)
}

fn setup_merger<R: Rng + CryptoRng>(rng: &mut R, path: &Path) -> Result<(), NanoZKPError> {
    // Load the verifying key from file.
    let mut file = File::open(path.join("verifying_keys").join("macro_block_wrapper.bin"))?;

    let vk_macro_block_wrapper = VerifyingKey::deserialize_uncompressed_unchecked(&mut file)?;

    // Create dummy inputs.
    let proof_merger_wrapper = Proof {
        a: G1MNT6::rand(rng).into_affine(),
        b: G2MNT6::rand(rng).into_affine(),
        c: G1MNT6::rand(rng).into_affine(),
    };

    let proof_macro_block_wrapper = Proof {
        a: G1MNT6::rand(rng).into_affine(),
        b: G2MNT6::rand(rng).into_affine(),
        c: G1MNT6::rand(rng).into_affine(),
    };

    let vk_merger_wrapper = VerifyingKey {
        alpha_g1: G1MNT6::rand(rng).into_affine(),
        beta_g2: G2MNT6::rand(rng).into_affine(),
        gamma_g2: G2MNT6::rand(rng).into_affine(),
        delta_g2: G2MNT6::rand(rng).into_affine(),
        gamma_abc_g1: vec![G1MNT6::rand(rng).into_affine(); 7],
    };

    let mut intermediate_state_commitment = [0u8; 95];
    rng.fill_bytes(&mut intermediate_state_commitment);

    let genesis_flag = bool::rand(rng);

    let mut initial_state_commitment = [0u8; 95];
    rng.fill_bytes(&mut initial_state_commitment);

    let mut final_state_commitment = [0u8; 95];
    rng.fill_bytes(&mut final_state_commitment);

    let mut vk_commitment = [0u8; 95];
    rng.fill_bytes(&mut vk_commitment);

    // Create parameters for our circuit
    let circuit = MergerCircuit::new(
        vk_macro_block_wrapper,
        proof_merger_wrapper,
        proof_macro_block_wrapper,
        vk_merger_wrapper,
        intermediate_state_commitment,
        genesis_flag,
        initial_state_commitment,
        final_state_commitment,
        vk_commitment,
    );

    let (pk, vk) = Groth16::<MNT4_753>::setup(circuit, rng)?;

    // Save keys to file.
    keys_to_file(&pk, &vk, "merger", path)
}

fn setup_merger_wrapper<R: Rng + CryptoRng>(rng: &mut R, path: &Path) -> Result<(), NanoZKPError> {
    // Load the verifying key from file.
    let mut file = File::open(path.join("verifying_keys").join("merger.bin"))?;

    let vk_merger = VerifyingKey::deserialize_uncompressed_unchecked(&mut file)?;

    // Create dummy inputs.
    let proof = Proof {
        a: G1MNT4::rand(rng).into_affine(),
        b: G2MNT4::rand(rng).into_affine(),
        c: G1MNT4::rand(rng).into_affine(),
    };

    let mut initial_state_commitment = [0u8; 95];
    rng.fill_bytes(&mut initial_state_commitment);

    let mut final_state_commitment = [0u8; 95];
    rng.fill_bytes(&mut final_state_commitment);

    let mut vk_commitment = [0u8; 95];
    rng.fill_bytes(&mut vk_commitment);

    // Create parameters for our circuit
    let circuit = MergerWrapperCircuit::new(
        vk_merger,
        proof,
        initial_state_commitment,
        final_state_commitment,
        vk_commitment,
    );

    let (pk, vk) = Groth16::<MNT6_753>::setup(circuit, rng)?;

    // Save keys to file.
    keys_to_file(&pk, &vk, "merger_wrapper", path)
}

fn keys_to_file<T: Pairing>(
    pk: &ProvingKey<T>,
    vk: &VerifyingKey<T>,
    name: &str,
    path: &Path,
) -> Result<(), NanoZKPError> {
    let verifying_keys = path.join("verifying_keys");
    let proving_keys = path.join("proving_keys");

    // Save proving key to file.
    if !proving_keys.is_dir() {
        DirBuilder::new().create(&proving_keys)?;
    }

    let mut file = File::create(proving_keys.join(format!("{name}.bin")))?;

    pk.serialize_uncompressed(&mut file)?;

    file.sync_all()?;

    // Save verifying key to file.
    if !verifying_keys.is_dir() {
        DirBuilder::new().create(&verifying_keys)?;
    }

    let mut file = File::create(verifying_keys.join(format!("{name}.bin")))?;

    vk.serialize_uncompressed(&mut file)?;

    file.sync_all()?;

    Ok(())
}
