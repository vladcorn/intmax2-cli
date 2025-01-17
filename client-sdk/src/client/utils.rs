use intmax2_zkp::{
    common::{salt::Salt, transfer::Transfer, trees::transfer_tree::TransferTree},
    constants::{NUM_TRANSFERS_IN_TX, TRANSFER_TREE_HEIGHT},
};

pub fn generate_salt() -> Salt {
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
