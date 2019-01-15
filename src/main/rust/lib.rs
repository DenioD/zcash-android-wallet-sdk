extern crate failure;

#[macro_use]
extern crate log;

extern crate ff;
extern crate pairing;
extern crate rusqlite;
extern crate sapling_crypto;
extern crate zcash_client_backend;
extern crate zcash_primitives;
extern crate zip32;

use failure::Error;
use ff::{PrimeField, PrimeFieldRepr};
use pairing::bls12_381::Bls12;
use rusqlite::{types::ToSql, Connection, NO_PARAMS};
use sapling_crypto::{
    jubjub::fs::{Fs, FsRepr},
    primitives::{Diversifier, Note},
};
use zcash_client_backend::{
    address::{decode_payment_address, encode_payment_address},
    constants::HRP_SAPLING_EXTENDED_SPENDING_KEY_TEST,
    prover::TxProver,
    transaction::Builder,
    welding_rig::scan_block_from_bytes,
};
use zcash_primitives::{
    merkle_tree::{CommitmentTree, IncrementalWitness},
    transaction::components::Amount,
    JUBJUB,
};
use zip32::{ChildIndex, ExtendedFullViewingKey, ExtendedSpendingKey};

const SAPLING_CONSENSUS_BRANCH_ID: u32 = 0x76b8_09bb;

const ANCHOR_OFFSET: u32 = 10;

fn extfvk_from_seed(seed: &[u8]) -> ExtendedFullViewingKey {
    let master = ExtendedSpendingKey::master(seed);
    let extsk = ExtendedSpendingKey::from_path(
        &master,
        &[
            ChildIndex::Hardened(32),
            ChildIndex::Hardened(1),
            ChildIndex::Hardened(0),
        ],
    );
    ExtendedFullViewingKey::from(&extsk)
}

fn address_from_extfvk(extfvk: &ExtendedFullViewingKey) -> String {
    let addr = extfvk.default_address().unwrap().1;
    encode_payment_address(HRP_SAPLING_EXTENDED_SPENDING_KEY_TEST, &addr)
}

fn init_data_database(db_data: &str) -> rusqlite::Result<()> {
    let data = Connection::open(db_data)?;
    data.execute(
        "CREATE TABLE IF NOT EXISTS blocks (
            height INTEGER PRIMARY KEY,
            time INTEGER,
            sapling_tree BLOB
        )",
        NO_PARAMS,
    )?;
    data.execute(
        "CREATE TABLE IF NOT EXISTS transactions (
            id_tx INTEGER PRIMARY KEY,
            txid BLOB NOT NULL UNIQUE,
            block INTEGER,
            raw BLOB,
            FOREIGN KEY (block) REFERENCES blocks(height)
        )",
        NO_PARAMS,
    )?;
    data.execute(
        "CREATE TABLE IF NOT EXISTS received_notes (
            id_note INTEGER PRIMARY KEY,
            tx INTEGER NOT NULL,
            output_index INTEGER NOT NULL,
            account INTEGER NOT NULL,
            diversifier BLOB NOT NULL,
            value INTEGER NOT NULL,
            rcm BLOB NOT NULL,
            nf BLOB NOT NULL UNIQUE,
            is_change BOOLEAN NOT NULL,
            memo BLOB,
            spent INTEGER,
            FOREIGN KEY (tx) REFERENCES transactions(id_tx),
            FOREIGN KEY (spent) REFERENCES transactions(id_tx),
            CONSTRAINT tx_output UNIQUE (tx, output_index)
        )",
        NO_PARAMS,
    )?;
    data.execute(
        "CREATE TABLE IF NOT EXISTS sapling_witnesses (
            id_witness INTEGER PRIMARY KEY,
            note INTEGER NOT NULL,
            block INTEGER NOT NULL,
            witness BLOB NOT NULL,
            FOREIGN KEY (note) REFERENCES received_notes(id_note),
            FOREIGN KEY (block) REFERENCES blocks(height),
            CONSTRAINT witness_height UNIQUE (note, block)
        )",
        NO_PARAMS,
    )?;
    Ok(())
}

struct CompactBlockRow {
    height: i32,
    data: Vec<u8>,
}

#[derive(Clone)]
struct WitnessRow {
    id_note: i64,
    witness: IncrementalWitness,
}

fn get_balance(db_data: &str, account: u32) -> Result<Amount, Error> {
    let data = Connection::open(db_data)?;

    let balance = data.query_row(
        "SELECT SUM(value) FROM received_notes
        WHERE account = ? AND spent IS NULL",
        &[account],
        |row| row.get(0),
    )?;

    Ok(Amount(balance))
}

/// Scans new blocks added to the cache for any transactions received by the given
/// ExtendedFullViewingKeys.
///
/// Assumes that the caller is handling rollbacks.
fn scan_cached_blocks(
    db_cache: &str,
    db_data: &str,
    extfvks: &[ExtendedFullViewingKey],
    birthday: i32,
) -> Result<(), Error> {
    let cache = Connection::open(db_cache)?;
    let data = Connection::open(db_data)?;

    // Recall where we synced up to previously
    let mut last_height =
        data.query_row(
            "SELECT MAX(height) FROM blocks",
            NO_PARAMS,
            |row| match row.get_checked(0) {
                Ok(h) => h,
                Err(_) => birthday,
            },
        )?;

    // Prepare necessary SQL statements
    let mut stmt_blocks = cache
        .prepare("SELECT height, data FROM compactblocks WHERE height > ? ORDER BY height ASC")?;
    let mut stmt_fetch_tree = data.prepare("SELECT sapling_tree FROM blocks WHERE height = ?")?;
    let mut stmt_fetch_witnesses =
        data.prepare("SELECT note, witness FROM sapling_witnesses WHERE block = ?")?;
    let mut stmt_fetch_nullifiers =
        data.prepare("SELECT id_note, nf, account FROM received_notes WHERE spent IS NULL")?;
    let mut stmt_insert_block = data.prepare(
        "INSERT INTO blocks (height, sapling_tree)
        VALUES (?, ?)",
    )?;
    let mut stmt_update_tx = data.prepare("UPDATE transactions SET block = ? WHERE txid = ?")?;
    let mut stmt_insert_tx = data.prepare(
        "INSERT INTO transactions (txid, block)
        VALUES (?, ?)",
    )?;
    let mut stmt_select_tx = data.prepare("SELECT id_tx FROM transactions WHERE txid = ?")?;
    let mut stmt_mark_spent_note =
        data.prepare("UPDATE received_notes SET spent = ? WHERE nf = ?")?;
    let mut stmt_insert_note = data.prepare(
        "INSERT INTO received_notes (tx, output_index, account, diversifier, value, rcm, nf, is_change)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    let mut stmt_insert_witness = data.prepare(
        "INSERT INTO sapling_witnesses (note, block, witness)
        VALUES (?, ?, ?)",
    )?;
    let mut stmt_prune_witnesses = data.prepare("DELETE FROM sapling_witnesses WHERE block < ?")?;

    // Fetch the CompactBlocks we need to scan
    let rows = stmt_blocks.query_map(&[last_height], |row| CompactBlockRow {
        height: row.get(0),
        data: row.get(1),
    })?;

    // Get the most recent CommitmentTree
    let mut tree = match stmt_fetch_tree.query_row(&[last_height], |row| match row.get_checked(0) {
        Ok(data) => {
            let data: Vec<_> = data;
            CommitmentTree::read(&data[..]).unwrap()
        }
        Err(_) => CommitmentTree::new(),
    }) {
        Ok(tree) => tree,
        Err(_) => CommitmentTree::new(),
    };

    // Get most recent incremental witnesses for the notes we are tracking
    let witnesses = stmt_fetch_witnesses.query_map(&[last_height], |row| {
        let data: Vec<_> = row.get(1);
        WitnessRow {
            id_note: row.get(0),
            witness: IncrementalWitness::read(&data[..]).unwrap(),
        }
    })?;
    let mut witnesses: Vec<_> = witnesses.into_iter().collect::<Result<_, _>>()?;

    // Get the nullifiers for the notes we are tracking
    let nullifiers = stmt_fetch_nullifiers.query_map(NO_PARAMS, |row| {
        let nf: Vec<_> = row.get(1);
        let account: i64 = row.get(2);
        (nf, account as usize)
    })?;
    let mut nullifiers: Vec<_> = nullifiers.into_iter().collect::<Result<_, _>>()?;

    for row in rows {
        let row = row?;

        // Start an SQL transaction for this block.
        data.execute("BEGIN IMMEDIATE", NO_PARAMS)?;

        // Scanned blocks MUST be height-seqential.
        if row.height != (last_height + 1) {
            error!(
                "Expected height of next CompactBlock to be {}, but was {}",
                last_height + 1,
                row.height
            );
            // Nothing more we can do
            break;
        }
        last_height = row.height;

        let txs = {
            let nf_refs: Vec<_> = nullifiers.iter().map(|(nf, acc)| (&nf[..], *acc)).collect();
            let mut witness_refs: Vec<_> = witnesses.iter_mut().map(|w| &mut w.witness).collect();
            scan_block_from_bytes(
                &row.data,
                &extfvks,
                &nf_refs,
                &mut tree,
                &mut witness_refs[..],
            )
        };

        // Insert the block into the database.
        let mut encoded_tree = Vec::new();
        tree.write(&mut encoded_tree).unwrap();
        stmt_insert_block.execute(&[row.height.to_sql()?, encoded_tree.to_sql()?])?;

        for (tx, new_witnesses) in txs {
            // First try update an existing transaction in the database.
            let txid = tx.txid.0.to_vec();
            let tx_row = if stmt_update_tx.execute(&[row.height.to_sql()?, txid.to_sql()?])? == 0 {
                // It isn't there, so insert our transaction into the database.
                stmt_insert_tx.execute(&[txid.to_sql()?, row.height.to_sql()?])?;
                data.last_insert_rowid()
            } else {
                // It was there, so grab its row number.
                stmt_select_tx.query_row(&[txid], |row| row.get(0))?
            };

            // Mark notes as spent and remove them from the scanning cache
            for spend in &tx.shielded_spends {
                stmt_mark_spent_note.execute(&[tx_row.to_sql()?, spend.nf.to_sql()?])?;
            }
            nullifiers = nullifiers
                .into_iter()
                .filter(|(nf, _acc)| {
                    tx.shielded_spends
                        .iter()
                        .find(|spend| &spend.nf == nf)
                        .is_none()
                })
                .collect();

            for (output, witness) in tx
                .shielded_outputs
                .into_iter()
                .zip(new_witnesses.into_iter())
            {
                let mut rcm = [0; 32];
                output.note.r.into_repr().write_le(&mut rcm[..])?;
                let nf = output.note.nf(
                    &extfvks[output.account].fvk.vk,
                    witness.position() as u64,
                    &JUBJUB,
                );

                // Insert received note into the database.
                // Assumptions:
                // - A transaction will not contain more than 2^63 shielded outputs.
                // - A note value will never exceed 2^63 zatoshis.
                stmt_insert_note.execute(&[
                    tx_row.to_sql()?,
                    (output.index as i64).to_sql()?,
                    (output.account as i64).to_sql()?,
                    output.to.diversifier.0.to_sql()?,
                    (output.note.value as i64).to_sql()?,
                    rcm.to_sql()?,
                    nf.to_sql()?,
                    output.is_change.to_sql()?,
                ])?;
                let note_row = data.last_insert_rowid();

                // Save witness for note.
                witnesses.push(WitnessRow {
                    id_note: note_row,
                    witness,
                });

                // Cache nullifier for note (to detect subsequent spends in this scan).
                nullifiers.push((nf, output.account));
            }
        }

        // Insert current witnesses into the database.
        let mut encoded = Vec::new();
        for witness_row in witnesses.iter() {
            encoded.clear();
            witness_row.witness.write(&mut encoded).unwrap();
            stmt_insert_witness.execute(&[
                witness_row.id_note.to_sql()?,
                last_height.to_sql()?,
                encoded.to_sql()?,
            ])?;
        }

        // Prune the stored witnesses (we only expect rollbacks of at most 100 blocks).
        stmt_prune_witnesses.execute(&[last_height - 100])?;

        // Commit the SQL transaction, writing this block's data atomically.
        data.execute("COMMIT", NO_PARAMS)?;
    }

    Ok(())
}

struct SelectedNoteRow {
    diversifier: Diversifier,
    note: Note<Bls12>,
    witness: IncrementalWitness,
}

/// Creates a transaction paying the specified address.
fn send_to_address(
    db_data: &str,
    consensus_branch_id: u32,
    master: &ExtendedSpendingKey,
    prover: impl TxProver,
    account: u32,
    to: &str,
    value: Amount,
) -> Result<i64, Error> {
    // Derive the ExtendedFullViewingKey for the account we are spending from.
    let extsk = ExtendedSpendingKey::from_path(
        &master,
        &[
            ChildIndex::Hardened(32),
            ChildIndex::Hardened(1),
            ChildIndex::Hardened(account),
        ],
    );
    let extfvk = ExtendedFullViewingKey::from(&extsk);

    let to = decode_payment_address(HRP_SAPLING_EXTENDED_SPENDING_KEY_TEST, to)?;

    let data = Connection::open(db_data)?;

    // Target the next block, assuming we are up-to-date.
    let height = data.query_row_and_then("SELECT MAX(height) FROM blocks", NO_PARAMS, |row| {
        let ret: Result<u32, _> = row.get_checked(0);
        ret
    })? + 1;

    // The goal of this SQL statement is to select the oldest notes until the required
    // value has been reached, and then fetch the witnesses at the desired height for the
    // selected notes. This is achieved in several steps:
    //
    // 1) Use a window function to create a view of all notes, ordered from oldest to
    //    newest, with an additional column containing a running sum:
    //    - Unspent notes accumulate the values of all unspent notes in that note's
    //      account, up to itself.
    //    - Spent notes accumulate the values of all notes in the transaction they were
    //      spent in, up to itself.
    //
    // 2) Select all unspent notes in the desired account, along with their running sum.
    //
    // 3) Select all notes for which the running sum was less than the required value, as
    //    well as a single note for which the sum was greater than or equal to the
    //    required value, bringing the sum of all selected notes across the threshold.
    //
    // 4) Match the selected notes against the witnesses at the desired height.
    let mut stmt_select_notes = data.prepare(
        "WITH selected AS (
            WITH eligible AS (
                SELECT id_note, diversifier, value, rcm,
                    SUM(value) OVER
                        (PARTITION BY account, spent ORDER BY id_note) AS so_far
                FROM received_notes
                WHERE account = ? AND spent IS NULL
            )
            SELECT * FROM eligible WHERE so_far < ?
            UNION
            SELECT * FROM (SELECT * FROM eligible WHERE so_far >= ? LIMIT 1)
        ), witnesses AS (
            SELECT note, witness FROM sapling_witnesses
            WHERE block = ?
        )
        SELECT selected.diversifier, selected.value, selected.rcm, witnesses.witness
        FROM selected
        INNER JOIN witnesses ON selected.id_note = witnesses.note",
    )?;

    // Select notes
    let notes = stmt_select_notes.query_and_then::<_, Error, _, _>(
        &[
            account as i64,
            value.0,
            value.0,
            (height - ANCHOR_OFFSET) as i64,
        ],
        |row| {
            let mut diversifier = Diversifier([0; 11]);
            let d: Vec<_> = row.get(0);
            diversifier.0.copy_from_slice(&d);

            let note_value: i64 = row.get(1);

            let d: Vec<_> = row.get(2);
            let rcm = {
                let mut tmp = FsRepr::default();
                tmp.read_le(&d[..])?;
                Fs::from_repr(tmp)?
            };

            let from = extfvk
                .fvk
                .vk
                .into_payment_address(diversifier, &JUBJUB)
                .unwrap();
            let note = from.create_note(note_value as u64, rcm, &JUBJUB).unwrap();

            let d: Vec<_> = row.get(3);
            let witness = IncrementalWitness::read(&d[..])?;

            Ok(SelectedNoteRow {
                diversifier,
                note,
                witness,
            })
        },
    )?;

    // Create the transaction
    let mut builder = Builder::new(1, height);
    for selected in notes {
        let selected = selected?;
        builder.add_sapling_spend(
            account,
            selected.diversifier,
            selected.note,
            selected.witness,
        )?;
    }
    builder.add_sapling_output(account, to, value, None)?;
    let tx = builder.build(consensus_branch_id, master, prover)?;

    // Save the transaction in the database.
    let mut raw_tx = vec![];
    tx.write(&mut raw_tx)?;
    let mut stmt_insert_tx = data.prepare(
        "INSERT INTO transactions (txid, raw)
        VALUES (?, ?)",
    )?;
    stmt_insert_tx.execute(&[&tx.txid().0[..], &raw_tx[..]])?;

    // Return the row number of the transaction, so the caller can fetch it for sending.
    Ok(data.last_insert_rowid())
}

/// JNI interface
#[cfg(target_os = "android")]
#[allow(non_snake_case)]
pub mod android {
    extern crate android_logger;
    extern crate jni;
    extern crate log_panics;

    use log::Level;
    use std::path::Path;
    use zcash_client_backend::prover::LocalTxProver;
    use zcash_primitives::transaction::components::Amount;
    use zip32::ExtendedSpendingKey;

    use self::android_logger::Filter;
    use self::jni::objects::{JClass, JString};
    use self::jni::sys::{jboolean, jbyteArray, jint, jlong, jstring, JNI_FALSE, JNI_TRUE};
    use self::jni::JNIEnv;

    use super::{
        address_from_extfvk, extfvk_from_seed, get_balance, init_data_database, scan_cached_blocks,
        send_to_address, SAPLING_CONSENSUS_BRANCH_ID,
    };

    #[no_mangle]
    pub unsafe extern "C" fn Java_cash_z_wallet_sdk_jni_JniConverter_initLogs(
        _env: JNIEnv,
        _: JClass,
    ) {
        android_logger::init_once(
            Filter::default().with_min_level(Level::Trace),
            Some("cash.z.rust.logs"),
        );

        log_panics::init();

        debug!("logs have been initialized successfully");
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_cash_z_wallet_sdk_jni_JniConverter_initDataDb(
        env: JNIEnv,
        _: JClass,
        db_data: JString,
    ) -> jboolean {
        let db_data: String = env
            .get_string(db_data)
            .expect("Couldn't get Java string!")
            .into();

        match init_data_database(&db_data) {
            Ok(()) => JNI_TRUE,
            Err(e) => {
                error!("Error while initializing data DB: {}", e);
                JNI_FALSE
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_cash_z_wallet_sdk_jni_JniConverter_getAddress(
        env: JNIEnv,
        _: JClass,
        seed: jbyteArray,
    ) -> jstring {
        let seed = env.convert_byte_array(seed).unwrap();

        let addr = address_from_extfvk(&extfvk_from_seed(&seed));

        let output = env.new_string(addr).expect("Couldn't create Java string!");
        output.into_inner()
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_cash_z_wallet_sdk_jni_JniConverter_getBalance(
        env: JNIEnv,
        _: JClass,
        db_data: JString,
    ) -> jlong {
        let db_data: String = env
            .get_string(db_data)
            .expect("Couldn't get Java string!")
            .into();

        match get_balance(&db_data, 0) {
            Ok(balance) => balance.0,
            Err(e) => {
                error!("Error while fetching balance: {}", e);
                -1
            }
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_cash_z_wallet_sdk_jni_JniConverter_scanBlocks(
        env: JNIEnv,
        _: JClass,
        db_cache: JString,
        db_data: JString,
        seed: jbyteArray,
        birthday: jint,
    ) {
        let db_cache: String = env
            .get_string(db_cache)
            .expect("Couldn't get Java string!")
            .into();
        let db_data: String = env
            .get_string(db_data)
            .expect("Couldn't get Java string!")
            .into();
        let seed = env.convert_byte_array(seed).unwrap();

        if let Err(e) =
            scan_cached_blocks(&db_cache, &db_data, &[extfvk_from_seed(&seed)], birthday)
        {
            error!("Error while scanning blocks: {}", e);
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn Java_cash_z_wallet_sdk_jni_JniConverter_sendToAddress(
        env: JNIEnv,
        _: JClass,
        db_data: JString,
        seed: jbyteArray,
        to: JString,
        value: jlong,
        spend_params: JString,
        output_params: JString,
    ) -> jlong {
        let db_data: String = env
            .get_string(db_data)
            .expect("Couldn't get Java string!")
            .into();
        let seed = env.convert_byte_array(seed).unwrap();
        let to: String = env
            .get_string(to)
            .expect("Couldn't get Java string!")
            .into();
        let value = Amount(value);
        let spend_params: String = env
            .get_string(spend_params)
            .expect("Couldn't get Java string!")
            .into();
        let output_params: String = env
            .get_string(output_params)
            .expect("Couldn't get Java string!")
            .into();

        let prover = LocalTxProver::new(
            Path::new(&spend_params),
            "8270785a1a0d0bc77196f000ee6d221c9c9894f55307bd9357c3f0105d31ca63991ab91324160d8f53e2bbd3c2633a6eb8bdf5205d822e7f3f73edac51b2b70c",
            Path::new(&output_params),
            "657e3d38dbb5cb5e7dd2970e8b03d69b4787dd907285b5a7f0790dcc8072f60bf593b32cc2d1c030e00ff5ae64bf84c5c3beb84ddc841d48264b4a171744d028",
        );

        match send_to_address(
            &db_data,
            SAPLING_CONSENSUS_BRANCH_ID,
            &ExtendedSpendingKey::master(&seed),
            prover,
            0,
            &to,
            value,
        ) {
            Ok(tx_row) => tx_row,
            Err(e) => {
                error!("Error while sending funds: {}", e);
                -1
            }
        }
    }
}
