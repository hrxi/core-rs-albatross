use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::broadcast::Sender as BroadcastSender;
use tokio_stream::wrappers::BroadcastStream;

use beserial::Deserialize;
use nimiq_account::Account;
use nimiq_block::Block;
use nimiq_blockchain_interface::AbstractBlockchain;
use nimiq_blockchain_proxy::BlockchainProxy;
use nimiq_hash::Blake2bHash;
use nimiq_keys::Address;
use nimiq_network_interface::{
    network::{CloseReason, Network},
    peer_info::Services,
    request::{OutboundRequestError, RequestError},
};
use nimiq_primitives::{account::AccountType, key_nibbles::KeyNibbles, policy::Policy};
use nimiq_transaction::{
    extended_transaction::ExtendedTransaction, ControlTransactionTopic, Transaction,
    TransactionTopic,
};

use crate::messages::{
    RequestBlocksProof, RequestTransactionReceiptsByAddress, RequestTransactionsProof,
    RequestTrieProof, ResponseBlocksProof,
};
use crate::ConsensusEvent;

pub struct ConsensusProxy<N: Network> {
    pub blockchain: BlockchainProxy,
    pub network: Arc<N>,
    pub(crate) established_flag: Arc<AtomicBool>,
    pub(crate) events: BroadcastSender<ConsensusEvent>,
}

impl<N: Network> Clone for ConsensusProxy<N> {
    fn clone(&self) -> Self {
        Self {
            blockchain: self.blockchain.clone(),
            network: Arc::clone(&self.network),
            established_flag: Arc::clone(&self.established_flag),
            events: self.events.clone(),
        }
    }
}

impl<N: Network> ConsensusProxy<N> {
    pub async fn send_transaction(&self, tx: Transaction) -> Result<(), N::Error> {
        if tx.sender_type == AccountType::Staking || tx.recipient_type == AccountType::Staking {
            return self.network.publish::<ControlTransactionTopic>(tx).await;
        }
        self.network.publish::<TransactionTopic>(tx).await
    }

    pub fn is_established(&self) -> bool {
        self.established_flag.load(Ordering::Acquire)
    }

    pub fn subscribe_events(&self) -> BroadcastStream<ConsensusEvent> {
        BroadcastStream::new(self.events.subscribe())
    }

    pub async fn request_transaction_receipts_by_address(
        &self,
        address: Address,
        min_peers: usize,
        max: Option<u16>,
    ) -> Result<Vec<(Blake2bHash, u32)>, RequestError> {
        let mut obtained_receipts = HashSet::new();

        // We obtain a list of connected peers that could satisfy our request and perform the request to each one:
        for peer_id in self
            .get_peers_for_service(Services::TRANSACTION_INDEX, min_peers)
            .await?
        {
            log::debug!(
                peer_id = %peer_id,
                "Performing txn receipts by address request to peer",
            );
            let response = self
                .network
                .request::<RequestTransactionReceiptsByAddress>(
                    RequestTransactionReceiptsByAddress {
                        address: address.clone(),
                        max,
                    },
                    peer_id,
                )
                .await;

            match response {
                Ok(response) => {
                    log::debug!(
                        "Obtained txn receipts response, length {} ",
                        response.receipts.len()
                    );
                    obtained_receipts.extend(response.receipts);
                }
                Err(error) => {
                    // If there was a request error with this peer we log an error
                    log::error!(peer=%peer_id, err=%error,"There was an error requesting transaction receipts from peer");
                }
            }
        }

        let mut receipts: Vec<_> = obtained_receipts.into_iter().collect();
        receipts.sort_unstable_by_key(|receipt| receipt.1);
        receipts.reverse(); // Return newest receipts (highest block_number) first

        Ok(receipts)
    }

    pub async fn request_transactions_by_address(
        &self,
        address: Address,
        since_block_height: u32,
        ignored_hashes: Vec<Blake2bHash>,
        min_peers: usize,
        max: Option<u16>,
    ) -> Result<Vec<ExtendedTransaction>, RequestError> {
        let receipts = self
            .request_transaction_receipts_by_address(address, min_peers, max)
            .await?
            .into_iter()
            .filter(|(hash, block_number)| {
                block_number > &since_block_height && !ignored_hashes.contains(hash)
            })
            .collect();

        self.prove_transactions_from_receipts(receipts, min_peers)
            .await
    }

    pub async fn request_transaction_by_hash_and_block_number(
        &self,
        tx_hash: Blake2bHash,
        block_number: u32,
        min_peers: usize,
    ) -> Result<ExtendedTransaction, RequestError> {
        let receipts = vec![(tx_hash, block_number)];
        let mut txs = self
            .prove_transactions_from_receipts(receipts, min_peers)
            .await?;
        match txs.pop() {
            Some(tx) => Ok(tx),
            None => Err(RequestError::OutboundRequest(
                OutboundRequestError::NoReceiver,
            )),
        }
    }

    async fn get_peers_for_service(
        &self,
        services: Services,
        min_peers: usize,
    ) -> Result<Vec<<N as Network>::PeerId>, RequestError> {
        // First we tell the network to provide us with a vector that contains all the connected peers that support such services
        // Note: If the network could not provide enough peers that satisfies our requirement, then an error would be returned
        self.network
            .get_peers_by_services(services, min_peers)
            .await
            .map_err(|error| {
                log::error!(
                    err = %error,
                    "The request couldn't be fulfilled"
                );

                RequestError::OutboundRequest(OutboundRequestError::SendError)
            })
    }

    async fn prove_transactions_from_receipts(
        &self,
        receipts: Vec<(Blake2bHash, u32)>,
        min_peers: usize,
    ) -> Result<Vec<ExtendedTransaction>, RequestError> {
        // Group transaction hashes by the block number that proves those transactions to reduce the number of requests
        // There are three categories of block numbers:
        //  - Finalized epochs: we use the election block number that finalized the respective epoch
        //  - Finalized batch in the current epoch: We use the latest checkpoint block number
        //  - Current batch: We use the current head to prove those transactions

        let blockchain = self.blockchain.read();
        let election_head_number = blockchain.election_head().block_number();
        let checkpoint_head_number = blockchain.macro_head().block_number();
        let current_head_number = blockchain.head().block_number();

        // We drop the blockchain lock because it's no longer needed while we request proofs
        drop(blockchain);

        let mut verified_transactions = HashMap::new();

        // We obtain a list of connected peers that could satisfy our request and perform the request to each one:
        for peer_id in self
            .get_peers_for_service(Services::TRANSACTION_INDEX, min_peers)
            .await?
        {
            // This is the structure where we group transactions by their proving block number
            let mut hashes_by_block = HashMap::new();

            for (hash, block_number) in &receipts {
                // If the transaction was already verified, then we don't need to verify it again
                if verified_transactions.contains_key(hash) {
                    continue;
                }

                if block_number <= &election_head_number {
                    // First Case: Transactions from finalized epochs
                    hashes_by_block
                        .entry(Policy::election_block_after(*block_number))
                        .or_insert(vec![])
                        .push(hash.clone());
                } else if block_number <= &checkpoint_head_number {
                    // Second Case: Transactions from a finalized batch in the current epoch
                    hashes_by_block
                        .entry(checkpoint_head_number)
                        .or_insert(vec![])
                        .push(hash.clone());
                } else {
                    // Third Case: Transanctions from the current batch
                    hashes_by_block
                        .entry(current_head_number)
                        .or_insert(vec![])
                        .push(hash.clone());
                }
            }

            // Now we request proofs for each block and its hashes, according to its classification
            for (block_number, hashes) in hashes_by_block {
                log::debug!(
                block_number=%block_number,
                "Performing txn proof request for block number",
                    );
                let response = self
                    .network
                    .request::<RequestTransactionsProof>(
                        RequestTransactionsProof {
                            hashes,
                            block_number,
                        },
                        peer_id,
                    )
                    .await;
                match response {
                    Ok(proof_response) => {
                        // We verify the transaction using the proof
                        if let Some(proof) = proof_response.proof {
                            if let Some(block) = proof_response.block {
                                log::debug!(peer=%peer_id,"New txns proof and block from peer");
                                let mut verification_result = proof
                                    .verify(block.history_root().clone())
                                    .map_or(false, |result| result);

                                // Verify block inclusion
                                if block.block_number() <= election_head_number {
                                    // Cache election head such that it can't change between request and response
                                    let election_head = self.blockchain.read().election_head();

                                    // Request block inclusion proofs for txs of previous epochs
                                    let block_proof = {
                                        if let Ok(ResponseBlocksProof {
                                            proof: Some(block_proof),
                                        }) = self
                                            .network
                                            .request::<RequestBlocksProof>(
                                                RequestBlocksProof {
                                                    election_head: election_head.block_number(),
                                                    blocks: vec![block.block_number()],
                                                },
                                                peer_id,
                                            )
                                            .await
                                        {
                                            block_proof
                                        } else {
                                            log::debug!(peer=%peer_id, "Error requesting block proof");
                                            continue;
                                        }
                                    };

                                    // Verify that the block is part of the chain using the block inclusion proof
                                    if let Block::Macro(macro_block) = block {
                                        verification_result = verification_result
                                            && block_proof
                                                .is_block_proven(&election_head, &macro_block);
                                    } else {
                                        log::debug!(peer=%peer_id, "Macro block expected in tx proof response");
                                        continue;
                                    }
                                }

                                if verification_result {
                                    for tx in proof.history {
                                        verified_transactions.insert(tx.tx_hash(), tx);
                                    }
                                } else {
                                    // The proof didn't verify so we disconnect from this peer
                                    log::debug!(peer=%peer_id,"Disconnecting from peer because the transaction proof didn't verify");
                                    self.network
                                        .disconnect_peer(peer_id, CloseReason::Other)
                                        .await;
                                    break;
                                }
                            } else {
                                // If we receive a proof but we do not receive a block, we disconnect from the peer
                                log::debug!(peer=%peer_id,"Disconnecting from peer due to an inconsistency in the transaction proof response");
                                self.network
                                    .disconnect_peer(peer_id, CloseReason::Other)
                                    .await;
                                break;
                            }
                        } else {
                            log::debug!(peer=%peer_id, "We requested a transaction proof but the peer didn't provide any");
                        }
                    }
                    Err(error) => {
                        // If there was a request error with this peer we don't request anymore proofs from it
                        log::error!(peer=%peer_id, err=%error,"There was an error requesting transaction proof from peer");
                        break;
                    }
                }
            }
        }

        // Sort transactions by block_number
        let mut transactions: Vec<_> = verified_transactions.into_values().collect();
        transactions.sort_unstable_by_key(|ext_tx| ext_tx.block_number);
        transactions.reverse(); // Return newest transaction (highest block_number) first

        Ok(transactions)
    }

    pub async fn request_accounts_by_addresses(
        &self,
        addresses: Vec<Address>,
        min_peers: usize,
    ) -> Result<Vec<(Address, Account)>, RequestError> {
        // First we tell the network to provide us with a vector that contains all the connected peers that support such services
        // Note: If the network could not provide enough peers that satisfies our requirement, then an error would be returned
        let peers = self
            .network
            .get_peers_by_services(Services::ACCOUNTS_PROOF, min_peers)
            .await
            .map_err(|error| {
                log::error!(
                    err = %error,
                    "Request accounts by addresses couldn't be fulfilled"
                );

                RequestError::OutboundRequest(OutboundRequestError::SendError)
            })?;

        let mut verified_accounts = HashMap::new();

        for peer_id in peers {
            // If we already verified all accounts we are done
            if addresses.len() == verified_accounts.len() {
                break;
            }

            let mut unverified_keys: HashMap<KeyNibbles, Address> = HashMap::from_iter(
                addresses
                    .iter()
                    .filter(|&address| !verified_accounts.contains_key(address))
                    .map(|address| (KeyNibbles::from(address), address.clone())),
            );

            log::debug!(
                peer_id = %peer_id,
                "Performing accounts by address request to peer",
            );
            let keys_to_verify: Vec<KeyNibbles> = unverified_keys.keys().cloned().collect();
            let response = self
                .network
                .request::<RequestTrieProof>(
                    RequestTrieProof {
                        keys: keys_to_verify.clone(),
                    },
                    peer_id,
                )
                .await;

            match response {
                Ok(response) => {
                    if let Some(proof) = response.proof {
                        if let Some(block_hash) = response.block_hash {
                            let blockchain = self.blockchain.read();
                            // First try to obtain, from our chain store, the block that was used to generate the proof
                            let block = blockchain.get_block(&block_hash, false).ok();

                            if let Some(block) = block {
                                // Now we need to verify the proof
                                if let Ok(values) =
                                    proof.verify_values(block.state_root(), &keys_to_verify)
                                {
                                    // If the proof is valid, then we add the obtained accounts to our verified accounts vector.
                                    for (key, value) in values {
                                        if let Some(address) = unverified_keys.remove(&key) {
                                            if let Ok(account) =
                                                Account::deserialize_from_vec(&value)
                                            {
                                                verified_accounts.insert(address, account);
                                            }
                                        }
                                    }
                                } else {
                                    // If the proof does not verify, we disconnect from the peer
                                    log::debug!(peer=%peer_id,"Disconnecting from peer because the accounts proof didn't verify");
                                    self.network
                                        .disconnect_peer(peer_id, CloseReason::Other)
                                        .await;
                                    break;
                                }
                            } else {
                                // If we couldn't find the block, then we cannot verify the proof
                                log::debug!(block_hash=%block_hash,"Received an accounts proof, but we could not find the block that was used to generate the proof");
                            }
                        } else {
                            // The peer provided a proof but did not provide the block hash, this is considered malicious
                            log::debug!(peer=%peer_id,"Disconnecting from peer because of mailicious behaviour during accounts proof");
                            self.network
                                .disconnect_peer(peer_id, CloseReason::MaliciousPeer)
                                .await;
                        }
                    } else {
                        // If there is no proof, then we just continue with the next peer
                        log::debug!(peer=%peer_id, "We requested an accounts proof but the peer didn't provide any");
                    }
                }
                Err(error) => {
                    // If there was a request error with this peer we log an error
                    log::error!(peer=%peer_id, err=%error,"There was an error requesting accounts proof from peer");
                }
            }
        }

        Ok(verified_accounts.into_iter().collect())
    }
}
