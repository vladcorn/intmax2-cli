use ethers::types::H256;
use intmax2_zkp::{
    circuits::balance::{balance_pis::BalancePublicInputs, send::spent_circuit::SpentPublicInputs},
    common::{
        block_builder::BlockProposal,
        deposit::{get_pubkey_salt_hash, Deposit},
        salt::Salt,
        signature::key_set::KeySet,
        transfer::Transfer,
        trees::transfer_tree::TransferTree,
        tx::Tx,
        witness::{
            spent_witness::SpentWitness, transfer_witness::TransferWitness,
            withdrawal_witness::WithdrawalWitness,
        },
    },
    constants::{NUM_TRANSFERS_IN_TX, TRANSFER_TREE_HEIGHT},
    ethereum_types::u256::U256,
    mock::data::{
        common_tx_data::CommonTxData, deposit_data::DepositData, meta_data::MetaData,
        transfer_data::TransferData, tx_data::TxData, user_data::UserData,
    },
};

use plonky2::{
    field::goldilocks_field::GoldilocksField,
    plonk::{config::PoseidonGoldilocksConfig, proof::ProofWithPublicInputs},
};

use crate::{
    client::balance_logic::{process_common_tx, process_transfer},
    external_api::{
        balance_prover::interface::BalanceProverInterface,
        block_builder::interface::BlockBuilderInterface,
        block_validity_prover::interface::BlockValidityInterface,
        contract::interface::ContractInterface, store_vault_server::interface::StoreVaultInterface,
    },
};

use super::{
    balance_logic::process_deposit,
    error::ClientError,
    strategy::{
        strategy::{determin_next_action, Action},
        withdrawal::fetch_withdrawal_info,
    },
};

type F = GoldilocksField;
type C = PoseidonGoldilocksConfig;
const D: usize = 2;

pub struct Client<
    BC: ContractInterface,
    BB: BlockBuilderInterface,
    S: StoreVaultInterface,
    V: BlockValidityInterface,
    B: BalanceProverInterface,
> {
    pub contract: BC,
    pub block_builder: BB,
    pub store_vault_server: S,
    pub validity_prover: V,
    pub balance_prover: B,

    // config
    pub deposit_timeout: u64,
    pub tx_timeout: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyncStatus {
    Continue, // continue syncing
    Complete, // sync completed
    Pending,  // there are pending actions
}

impl<BC, BB, S, V, B> Client<BC, BB, S, V, B>
where
    BC: ContractInterface,
    BB: BlockBuilderInterface,
    S: StoreVaultInterface,
    V: BlockValidityInterface,
    B: BalanceProverInterface,
{
    pub async fn deposit(
        &self,
        rpc_url: &str,
        ethereum_private_key: H256,
        key: KeySet,
        token_index: u32,
        amount: U256,
    ) -> Result<(), ClientError> {
        if token_index != 0 {
            todo!("multiple token support")
        }

        // todo: improve the way to choose deposit salt
        let deposit_salt = generate_salt(key, 0);

        // backup before contract call
        let pubkey_salt_hash = get_pubkey_salt_hash(key.pubkey, deposit_salt);
        let deposit = Deposit {
            pubkey_salt_hash,
            token_index,
            amount,
        };
        let deposit_data = DepositData {
            deposit_salt,
            deposit,
        };
        self.store_vault_server
            .save_deposit_data(key.pubkey, deposit_data.encrypt(key.pubkey))
            .await?;

        // call contract
        self.contract
            .deposit_native_token(rpc_url, ethereum_private_key, pubkey_salt_hash, amount)
            .await?;

        Ok(())
    }

    pub async fn send_tx(&self, key: KeySet, transfers: Vec<Transfer>) -> Result<(), ClientError> {
        // input validation
        if transfers.len() == 0 {
            return Err(ClientError::InternalError("transfers is empty".to_string()));
        }
        if transfers.len() > NUM_TRANSFERS_IN_TX {
            return Err(ClientError::InternalError(
                "transfers is too long".to_string(),
            ));
        }

        // sync balance proof
        self.sync(key).await?;

        let user_data = self.get_user_data(key).await?;
        let _balance_proof = self
            .store_vault_server
            .get_balance_proof(
                key.pubkey,
                user_data.block_number,
                user_data.private_commitment(),
            )
            .await?
            .ok_or_else(|| ClientError::InternalError("balance proof not found".to_string()))?;

        // balance check
        let balances = user_data.balances();
        for transfer in &transfers {
            let balance = balances
                .get(&(transfer.token_index as usize))
                .cloned()
                .unwrap_or_default();
            if balance.is_insufficient {
                return Err(ClientError::BalanceError(format!(
                    "Already insufficient balance: token index {}",
                    transfer.token_index
                )));
            }
            if balance.amount < transfer.amount {
                return Err(ClientError::BalanceError(format!(
                    "Insufficient balance: {} < {} for token index {}",
                    balance.amount, transfer.amount, transfer.token_index
                )));
            }
        }

        // generate spent proof
        let transfer_tree = generate_transfer_tree(&transfers);
        let tx = Tx {
            nonce: user_data.full_private_state.nonce,
            transfer_tree_root: transfer_tree.get_root(),
        };
        let new_salt = generate_salt(key, user_data.full_private_state.nonce);
        let spent_witness = SpentWitness::new(
            &user_data.full_private_state.asset_tree,
            &user_data.full_private_state.to_private_state(),
            &transfer_tree.leaves(), // this is padded
            tx,
            new_salt,
        )
        .map_err(|e| ClientError::WitnessGenerationError(format!("Spent witness: {:?}", e)))?;
        let spent_proof = self.balance_prover.prove_spent(&spent_witness).await?;

        let is_first_time = self
            .validity_prover
            .get_account_id(key.pubkey)
            .await?
            .is_none();

        // get block proposal & verify it
        // let proposals = block_builder
        //     .propose(
        //         contract,
        //         validity_prover,
        //         is_first_time, // Use registration block for the first tx
        //         vec![(key.pubkey, tx)],
        //     )
        //     .map_err(|e| anyhow::anyhow!("failed to propose block: {}", e))?;
        let proposal: BlockProposal = todo!();
        // proposal
        //     .verify(tx)
        //     .map_err(|e| anyhow::anyhow!("failed to verify proposal: {}", e))?;

        // backup before posting block
        let common_tx_data = CommonTxData {
            spent_proof,
            sender_prev_block_number: user_data.block_number,
            tx,
            tx_index: proposal.tx_index,
            tx_merkle_proof: proposal.tx_merkle_proof.clone(),
            tx_tree_root: proposal.tx_tree_root,
        };
        // save tx data
        let tx_data = TxData {
            common: common_tx_data.clone(),
            spent_witness,
        };
        self.store_vault_server
            .save_tx_data(key.pubkey, tx_data.encrypt(key.pubkey))
            .await?;
        // save transfer data
        for (i, transfer) in transfers.iter().enumerate() {
            let transfer_merkle_proof = transfer_tree.prove(i);
            let transfer_data = TransferData {
                sender: key.pubkey,
                prev_block_number: user_data.block_number,
                prev_private_commitment: user_data.private_commitment(),
                tx_data: common_tx_data.clone(),
                transfer: transfer.clone(),
                transfer_index: i,
                transfer_merkle_proof,
            };
            if transfer.recipient.is_pubkey {
                let recipient = transfer.recipient.to_pubkey().unwrap();
                self.store_vault_server
                    .save_transfer_data(
                        transfer.recipient.to_pubkey().unwrap(),
                        transfer_data.encrypt(recipient),
                    )
                    .await?;
            } else {
                self.store_vault_server
                    .save_withdrawal_data(key.pubkey, transfer_data.encrypt(key.pubkey))
                    .await?;
            }
        }

        // sign and post block
        let signature = proposal.sign(key);
        // self.block_builder
        //     .post_block(contract, validity_prover, vec![signature])
        //     .map_err(|e| anyhow::anyhow!("failed to post block: {}", e))?;
        Ok(())
    }

    pub async fn sync(&self, key: KeySet) -> Result<(), ClientError> {
        let mut sync_status = SyncStatus::Continue;
        while sync_status == SyncStatus::Continue {
            sync_status = self.sync_single(key).await?;
        }
        if sync_status == SyncStatus::Pending {
            todo!("handle pending actions")
        }
        Ok(())
    }

    pub async fn sync_single(&self, key: KeySet) -> Result<SyncStatus, ClientError> {
        let next_action = determin_next_action(
            &self.store_vault_server,
            &self.validity_prover,
            key,
            self.deposit_timeout,
            self.tx_timeout,
        )
        .await?;

        // if there are pending actions, return pending
        // todo: process non-pending actions if possible
        if next_action.pending_deposits.len() > 0
            || next_action.pending_transfers.len() > 0
            || next_action.pending_txs.len() > 0
        {
            return Ok(SyncStatus::Pending);
        }

        if next_action.action.is_none() {
            return Ok(SyncStatus::Complete);
        }

        match next_action.action.unwrap() {
            Action::Deposit(meta, deposit_data) => {
                self.sync_deposit(key, &meta, &deposit_data).await?;
            }
            Action::Transfer(meta, transfer_data) => {
                self.sync_transfer(key, &meta, &transfer_data).await?;
            }
            Action::Tx(meta, tx_data) => self.sync_tx(key, &meta, &tx_data).await?,
        }

        Ok(SyncStatus::Continue)
    }

    pub async fn sync_withdrawals(&self, key: KeySet) -> Result<(), ClientError> {
        let user_data = self.get_user_data(key).await?;

        let withdrawal_info = fetch_withdrawal_info(
            &self.store_vault_server,
            &self.validity_prover,
            key,
            user_data.withdrawal_lpt,
            self.tx_timeout,
        )
        .await?;
        if withdrawal_info.pending.len() > 0 {
            todo!("handle pending withdrawals")
        }
        for (meta, data) in &withdrawal_info.settled {
            self.sync_withdrawal(key, meta, data).await?;
        }
        Ok(())
    }

    async fn sync_deposit(
        &self,
        key: KeySet,
        meta: &MetaData,
        deposit_data: &DepositData,
    ) -> Result<(), ClientError> {
        let mut user_data = self.get_user_data(key).await?;

        // user's balance proof before applying the tx
        let prev_balance_proof = self
            .store_vault_server
            .get_balance_proof(
                key.pubkey,
                user_data.block_number,
                user_data.private_commitment(),
            )
            .await?;

        let new_salt = generate_salt(key, user_data.full_private_state.nonce);
        let new_balance_proof = process_deposit(
            &self.validity_prover,
            &self.balance_prover,
            user_data.pubkey,
            &mut user_data.full_private_state,
            new_salt,
            &prev_balance_proof,
            meta.block_number.unwrap(),
            deposit_data,
        )
        .await?;

        // update user data
        user_data.block_number = meta.block_number.unwrap();
        user_data.deposit_lpt = meta.timestamp;

        // save proof and user data
        self.store_vault_server
            .save_balance_proof(key.pubkey, new_balance_proof)
            .await?;
        self.store_vault_server
            .save_user_data(key.pubkey, user_data.encrypt(key.pubkey))
            .await?;

        Ok(())
    }

    async fn sync_transfer(
        &self,
        key: KeySet,
        meta: &MetaData,
        transfer_data: &TransferData<F, C, D>,
    ) -> Result<(), ClientError> {
        log::info!("sync_transfer: {:?}", meta);
        let mut user_data = self.get_user_data(key).await?;
        // user's balance proof before applying the tx
        let prev_balance_proof = self
            .store_vault_server
            .get_balance_proof(
                key.pubkey,
                user_data.block_number,
                user_data.private_commitment(),
            )
            .await?;

        // sender balance proof after applying the tx
        let new_sender_balance_proof = self
            .generate_new_sender_balance_proof(
                transfer_data.sender,
                meta.block_number.unwrap(),
                &transfer_data.tx_data,
            )
            .await?;

        let new_salt = generate_salt(key, user_data.full_private_state.nonce);
        let new_balance_proof = process_transfer(
            &self.validity_prover,
            &self.balance_prover,
            user_data.pubkey,
            &mut user_data.full_private_state,
            new_salt,
            &new_sender_balance_proof,
            &prev_balance_proof,
            meta.block_number.unwrap(),
            &transfer_data,
        )
        .await?;

        // update user data
        user_data.block_number = meta.block_number.unwrap();
        user_data.transfer_lpt = meta.timestamp;

        // save proof and user data
        self.store_vault_server
            .save_balance_proof(key.pubkey, new_balance_proof)
            .await?;
        self.store_vault_server
            .save_user_data(key.pubkey, user_data.encrypt(key.pubkey))
            .await?;

        Ok(())
    }

    async fn sync_tx(
        &self,
        key: KeySet,
        meta: &MetaData,
        tx_data: &TxData<F, C, D>,
    ) -> Result<(), ClientError> {
        log::info!("sync_tx: {:?}", meta);
        let mut user_data = self.get_user_data(key).await?;
        let balance_proof = self
            .generate_new_sender_balance_proof(
                key.pubkey,
                meta.block_number.unwrap(),
                &tx_data.common,
            )
            .await?;
        let balance_pis = BalancePublicInputs::from_pis(&balance_proof.public_inputs);
        if balance_pis.public_state.block_number != meta.block_number.unwrap() {
            return Err(ClientError::SyncError("block number mismatch".to_string()));
        }

        // update user data
        user_data.block_number = meta.block_number.unwrap();
        user_data.tx_lpt = meta.timestamp;
        tx_data
            .spent_witness
            .update_private_state(&mut user_data.full_private_state)
            .map_err(|e| {
                ClientError::InternalError(format!("failed to update private state: {}", e))
            })?;

        // validation
        if balance_pis.private_commitment != user_data.private_commitment() {
            return Err(ClientError::InternalError(
                "private commitment mismatch".to_string(),
            ));
        }

        // save user data
        self.store_vault_server
            .save_user_data(key.pubkey, user_data.encrypt(key.pubkey))
            .await?;
        Ok(())
    }

    async fn sync_withdrawal(
        &self,
        key: KeySet,
        meta: &MetaData,
        withdrawal_data: &TransferData<F, C, D>,
    ) -> Result<(), ClientError> {
        log::info!("sync_withdrawal: {:?}", meta);
        if meta.block_number.is_none() {
            return Err(ClientError::InternalError(
                "block number is not set".to_string(),
            ));
        }

        let mut user_data = self.get_user_data(key).await?;

        let new_user_balance_proof = self
            .generate_new_sender_balance_proof(
                key.pubkey,
                meta.block_number.unwrap(),
                &withdrawal_data.tx_data,
            )
            .await?;

        let withdrawal_witness = WithdrawalWitness {
            transfer_witness: TransferWitness {
                transfer: withdrawal_data.transfer.clone(),
                transfer_index: withdrawal_data.transfer_index,
                transfer_merkle_proof: withdrawal_data.transfer_merkle_proof.clone(),
                tx: withdrawal_data.tx_data.tx.clone(),
            },
            balance_proof: new_user_balance_proof,
        };
        let _single_withdrawal_proof = self
            .balance_prover
            .prove_single_withdrawal(&withdrawal_witness)
            .await?;

        // withdrawal_aggregator
        //     .add(&single_withdrawal_proof)
        //     .map_err(|e| anyhow::anyhow!("failed to add withdrawal: {}", e))?;

        // update user data
        user_data.block_number = meta.block_number.unwrap();
        user_data.withdrawal_lpt = meta.timestamp;

        // save user data
        self.store_vault_server
            .save_user_data(key.pubkey, user_data.encrypt(key.pubkey))
            .await?;

        Ok(())
    }

    // generate sender's balance proof after applying the tx
    // save the proof to the data store server
    async fn generate_new_sender_balance_proof(
        &self,
        sender: U256,
        block_number: u32,
        common_tx_data: &CommonTxData<F, C, D>,
    ) -> Result<ProofWithPublicInputs<F, C, D>, ClientError> {
        log::info!(
            "generate_new_sender_balance_proof: sender {}, block_number {}",
            sender,
            block_number
        );
        let spent_proof_pis =
            SpentPublicInputs::from_pis(&common_tx_data.spent_proof.public_inputs);

        let new_sender_balance_proof = self
            .store_vault_server
            .get_balance_proof(sender, block_number, spent_proof_pis.new_private_commitment)
            .await?;
        if new_sender_balance_proof.is_some() {
            // already updated
            return Ok(new_sender_balance_proof.unwrap());
        }

        let prev_sender_balance_proof = self
            .store_vault_server
            .get_balance_proof(
                sender,
                common_tx_data.sender_prev_block_number,
                spent_proof_pis.prev_private_commitment,
            )
            .await?
            .ok_or_else(|| {
                ClientError::InternalError("prev balance proof not found".to_string())
            })?;

        let new_sender_balance_proof = process_common_tx(
            &self.validity_prover,
            &self.balance_prover,
            sender,
            &Some(prev_sender_balance_proof),
            block_number,
            common_tx_data,
        )
        .await?;

        self.store_vault_server
            .save_balance_proof(sender, new_sender_balance_proof.clone())
            .await?;

        Ok(new_sender_balance_proof)
    }

    pub async fn get_user_data(&self, key: KeySet) -> Result<UserData, ClientError> {
        let user_data = self
            .store_vault_server
            .get_user_data(key.pubkey)
            .await?
            .map(|encrypted| UserData::decrypt(&encrypted, key))
            .transpose()
            .map_err(|e| {
                ClientError::DecryptionError(format!("failed to decrypt user data: {}", e))
            })?
            .unwrap_or(UserData::new(key.pubkey));
        Ok(user_data)
    }
}

pub fn generate_salt(_key: KeySet, _nonce: u32) -> Salt {
    // todo: deterministic salt generation
    let mut rng = rand::thread_rng();
    Salt::rand(&mut rng)
}

pub fn generate_transfer_tree(transfers: &[Transfer]) -> TransferTree {
    let mut transfers = transfers.to_vec();
    transfers.resize(NUM_TRANSFERS_IN_TX, Transfer::default());
    let mut transfer_tree = TransferTree::new(TRANSFER_TREE_HEIGHT);
    for transfer in &transfers {
        transfer_tree.push(transfer.clone());
    }
    transfer_tree
}