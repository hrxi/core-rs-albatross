use nimiq::consensus::base::account::tree::AccountsTree;
use nimiq::consensus::base::account::{Account, BasicAccount};
use nimiq::consensus::base::primitive::Address;
use hex;
use nimiq::utils::db::volatile::VolatileEnvironment;
use nimiq::utils::db::WriteTransaction;

#[test]
fn it_can_put_and_get_a_balance() {
    let address = Address::from(&hex::decode("0000000000000000000000000000000000000000").unwrap()[..]);
    let mut account = Account::Basic(BasicAccount { balance: 20 });

    let env = VolatileEnvironment::new();
    let tree = AccountsTree::new(&env);
    let mut txn = WriteTransaction::new(&env);

    // 1. Put account and check.
    tree.put(&mut txn, &address, account.clone());

    let account2 = tree.get(&txn, &address);
    assert!(account2.is_some());
    assert_eq!(account2.unwrap(), account);

    // 2. Increase balance, put, and check.
    if let Account::Basic(ref mut basic_account) = account {
        basic_account.balance = 50;
    }
    tree.put(&mut txn, &address, account.clone());

    let account2 = tree.get(&txn, &address);
    assert!(account2.is_some());
    assert_eq!(account2.unwrap(), account);

    // 3. Prune balance, put, and check.
    if let Account::Basic(ref mut basic_account) = account {
        basic_account.balance = 0;
    }
    tree.put(&mut txn, &address, account.clone());

    let account2 = tree.get(&txn, &address);
    assert!(account2.is_none());

    txn.abort();
}

#[test]
fn it_can_put_and_get_multiple_balances() {
    let address1 = Address::from(&hex::decode("0000000000000000000000000000000000000000").unwrap()[..]);
    let account1 = Account::Basic(BasicAccount { balance: 5 });
    let address2 = Address::from(&hex::decode("1000000000000000000000000000000000000000").unwrap()[..]);
    let account2 = Account::Basic(BasicAccount { balance: 55 });
    let address3 = Address::from(&hex::decode("1200000000000000000000000000000000000000").unwrap()[..]);
    let account3 = Account::Basic(BasicAccount { balance: 55555555 });

    let env = VolatileEnvironment::new();
    let tree = AccountsTree::new(&env);
    let mut txn = WriteTransaction::new(&env);

    // Put accounts and check.
    tree.put(&mut txn, &address1, account1.clone());
    tree.put(&mut txn, &address2, account2.clone());
    tree.put(&mut txn, &address3, account3.clone());


    let account1a = tree.get(&txn, &address1);
    assert!(account1a.is_some());
    assert_eq!(account1a.unwrap(), account1);

    let account2a = tree.get(&txn, &address2);
    assert!(account2a.is_some());
    assert_eq!(account2a.unwrap(), account2);

    let account3a = tree.get(&txn, &address3);
    assert!(account3a.is_some());
    assert_eq!(account3a.unwrap(), account3);

    txn.abort();
}

#[test]
fn it_is_invariant_to_history() {
    let address1 = Address::from(&hex::decode("0000000000000000000000000000000000000000").unwrap()[..]);
    let account1 = Account::Basic(BasicAccount { balance: 5 });
    let account2 = Account::Basic(BasicAccount { balance: 55 });

    let env = VolatileEnvironment::new();
    let tree = AccountsTree::new(&env);
    let mut txn = WriteTransaction::new(&env);

    tree.put(&mut txn, &address1, account1.clone());
    let root_hash1 = tree.root_hash(&txn);

    tree.put(&mut txn, &address1, account2.clone());
    let root_hash2 = tree.root_hash(&txn);
    assert_ne!(root_hash1, root_hash2);

    tree.put(&mut txn, &address1, account1.clone());
    let root_hash3 = tree.root_hash(&txn);
    assert_eq!(root_hash1, root_hash3);

    txn.abort();
}

#[test]
fn it_is_invariant_to_insertion_order() {
    let address1 = Address::from(&hex::decode("0000000000000000000000000000000000000000").unwrap()[..]);
    let account1 = Account::Basic(BasicAccount { balance: 5 });
    let address2 = Address::from(&hex::decode("1000000000000000000000000000000000000000").unwrap()[..]);
    let account2 = Account::Basic(BasicAccount { balance: 55 });
    let address3 = Address::from(&hex::decode("1200000000000000000000000000000000000000").unwrap()[..]);
    let account3 = Account::Basic(BasicAccount { balance: 55555555 });

    let empty_account = Account::Basic(BasicAccount { balance: 0 });

    let env = VolatileEnvironment::new();
    let tree = AccountsTree::new(&env);
    let mut txn = WriteTransaction::new(&env);

    // Order 1
    tree.put(&mut txn, &address1, account1.clone());
    tree.put(&mut txn, &address2, account2.clone());
    tree.put(&mut txn, &address3, account3.clone());
    let root_hash1 = tree.root_hash(&txn);

    // Reset
    tree.put(&mut txn, &address1, empty_account.clone());
    tree.put(&mut txn, &address2, empty_account.clone());
    tree.put(&mut txn, &address3, empty_account.clone());

    // Order 2
    tree.put(&mut txn, &address1, account1.clone());
    tree.put(&mut txn, &address3, account3.clone());
    tree.put(&mut txn, &address2, account2.clone());
    let root_hash2 = tree.root_hash(&txn);

    // Reset
    tree.put(&mut txn, &address1, empty_account.clone());
    tree.put(&mut txn, &address2, empty_account.clone());
    tree.put(&mut txn, &address3, empty_account.clone());

    // Order 3
    tree.put(&mut txn, &address2, account2.clone());
    tree.put(&mut txn, &address1, account1.clone());
    tree.put(&mut txn, &address3, account3.clone());
    let root_hash3 = tree.root_hash(&txn);

    // Reset
    tree.put(&mut txn, &address1, empty_account.clone());
    tree.put(&mut txn, &address2, empty_account.clone());
    tree.put(&mut txn, &address3, empty_account.clone());

    // Order 4
    tree.put(&mut txn, &address2, account2.clone());
    tree.put(&mut txn, &address3, account3.clone());
    tree.put(&mut txn, &address1, account1.clone());
    let root_hash4 = tree.root_hash(&txn);

    // Reset
    tree.put(&mut txn, &address1, empty_account.clone());
    tree.put(&mut txn, &address2, empty_account.clone());
    tree.put(&mut txn, &address3, empty_account.clone());

    // Order 5
    tree.put(&mut txn, &address3, account3.clone());
    tree.put(&mut txn, &address1, account1.clone());
    tree.put(&mut txn, &address2, account2.clone());
    let root_hash5 = tree.root_hash(&txn);

    // Reset
    tree.put(&mut txn, &address1, empty_account.clone());
    tree.put(&mut txn, &address2, empty_account.clone());
    tree.put(&mut txn, &address3, empty_account.clone());

    // Order 6
    tree.put(&mut txn, &address3, account3.clone());
    tree.put(&mut txn, &address2, account2.clone());
    tree.put(&mut txn, &address1, account1.clone());
    let root_hash6 = tree.root_hash(&txn);

    assert_eq!(root_hash1, root_hash2);
    assert_eq!(root_hash1, root_hash3);
    assert_eq!(root_hash1, root_hash4);
    assert_eq!(root_hash1, root_hash5);
    assert_eq!(root_hash1, root_hash6);

    txn.abort();
}

#[test]
fn it_can_merge_nodes_while_pruning() {
    let address1 = Address::from(&hex::decode("0102030405060708090a0b0c0d0e0f1011121314").unwrap()[..]);
    let account1 = Account::Basic(BasicAccount { balance: 5 });
    let address2 = Address::from(&hex::decode("0103030405060708090a0b0c0d0e0f1011121314").unwrap()[..]);
    let account2 = Account::Basic(BasicAccount { balance: 55 });
    let address3 = Address::from(&hex::decode("0103040405060708090a0b0c0d0e0f1011121314").unwrap()[..]);
    let account3 = Account::Basic(BasicAccount { balance: 55555555 });

    let empty_account = Account::Basic(BasicAccount { balance: 0 });

    let env = VolatileEnvironment::new();
    let tree = AccountsTree::new(&env);
    let mut txn = WriteTransaction::new(&env);

    tree.put(&mut txn, &address1, account1.clone());
    let root_hash1 = tree.root_hash(&txn);

    tree.put(&mut txn, &address2, account2.clone());
    tree.put(&mut txn, &address3, account3.clone());
    tree.put(&mut txn, &address2, empty_account.clone());
    tree.put(&mut txn, &address3, empty_account.clone());

    let root_hash2 = tree.root_hash(&txn);
    assert_eq!(root_hash1, root_hash2);

    txn.abort();
}
