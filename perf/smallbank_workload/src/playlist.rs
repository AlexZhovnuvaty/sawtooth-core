/*
 * Copyright 2017 Intel Corporation
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 * ------------------------------------------------------------------------------
 */

//! Tools for generating YAML playlists of transactions.

extern crate yaml_rust;
extern crate rand;
extern crate crypto;

use std::error;
use std::io::Read;
use std::io::Write;
use std::io::Error as StdIoError;
use std::fmt;
use std::borrow::Cow;
use std::time::Instant;

use self::yaml_rust::YamlEmitter;
use self::yaml_rust::YamlLoader;
use self::yaml_rust::Yaml;
use self::yaml_rust::EmitError;
use self::yaml_rust::yaml::Hash;
use self::rand::Rng;
use self::rand::StdRng;
use self::rand::SeedableRng;

use smallbank;
use smallbank::SmallbankTransactionPayload;
use smallbank::SmallbankTransactionPayload_PayloadType as SBPayloadType;

use protobuf;
use protobuf::Message;

use sawtooth_sdk::signing;
use sawtooth_sdk::messages::transaction::Transaction;
use sawtooth_sdk::messages::transaction::TransactionHeader;

use self::crypto::digest::Digest;
use self::crypto::sha2::Sha512;

macro_rules! yaml_map(
    { $($key:expr => $value:expr),+ } => {
        {
            let mut m = Hash::new();
            $(m.insert(Yaml::from_str($key), $value);)+
            Yaml::Hash(m)
        }
    };
);

/// Generates a playlist of Smallbank transactions.
///
/// This function generates a collection of smallbank transactions and writes
/// the result to the given output.  The resulting playlist will consist of
/// `num_accounts` CREATE_ACCOUNT transactions, followed by `num_transactions`
/// additional transactions (deposits, transfers, etc).
///
/// A random seed may be provided to create repeatable, random output.
pub fn generate_smallbank_playlist(output: &mut Write,
                                   num_accounts: usize,
                                   num_transactions: usize,
                                   seed: Option<i32>)
    -> Result<(), PlaylistError>
{
    let mut fmt_writer = FmtWriter::new(output);
    let mut emitter = YamlEmitter::new(&mut fmt_writer);

    let txn_array: Vec<Yaml> = create_smallbank_playlist(num_accounts, num_transactions, seed)
        .map(Yaml::from)
        .collect();

    let final_yaml = Yaml::Array(txn_array);
    try!(emitter.dump(&final_yaml).map_err(PlaylistError::YamlOutputError));

    Ok(())
}

/// Created signed Smallbank transactions from a given playlist.
///
/// The playlist input is expected to be the same Yaml format as generated by
/// the `generate_smallbank_playlist` function.  All transactions will be
/// signed with the given `PrivateKey` instance.
pub fn process_smallbank_playlist(output: &mut Write,
                                  playlist_input: &mut Read,
                                  signing_algorithm: &signing::Algorithm,
                                  signing_key: &signing::PrivateKey)
    -> Result<(), PlaylistError>
{
    let payloads = try!(read_smallbank_playlist(playlist_input));

    let crypto_factory = signing::CryptoFactory::new(signing_algorithm);
    let signer = crypto_factory.new_signer(signing_key);
    let pub_key = try!(signing_algorithm.get_public_key(signing_key).map_err(PlaylistError::SigningError));
    let pub_key_hex = pub_key.as_hex();

    let start = Instant::now();
    for payload in payloads {
        let mut txn = Transaction::new();
        let mut txn_header = TransactionHeader::new();

        txn_header.set_family_name(String::from("smallbank"));
        txn_header.set_family_version(String::from("1.0"));

        let elapsed = start.elapsed();
        txn_header.set_nonce(format!("{}{}", elapsed.as_secs(), elapsed.subsec_nanos()));

        let addresses = protobuf::RepeatedField::from_vec(make_addresses(&payload));

        txn_header.set_inputs(addresses.clone());
        txn_header.set_outputs(addresses.clone());

        let payload_bytes = try!(payload.write_to_bytes().map_err(PlaylistError::MessageError));

        let mut sha = Sha512::new();
        sha.input(&payload_bytes);
        let mut hash: &mut [u8] = & mut [0; 64];
        sha.result(hash);

        txn_header.set_payload_sha512(bytes_to_hex_str(hash));
        txn_header.set_signer_pubkey(pub_key_hex.clone());
        txn_header.set_batcher_pubkey(pub_key_hex.clone());

        let header_bytes = try!(txn_header.write_to_bytes().map_err(PlaylistError::MessageError));

        let signature = try!(signer.sign(&header_bytes).map_err(PlaylistError::SigningError));

        txn.set_header(header_bytes);
        txn.set_header_signature(signature);
        txn.set_payload(payload_bytes);

        try!(txn.write_length_delimited_to_writer(output).map_err(PlaylistError::MessageError))
    }

    Ok(())
}

fn make_addresses(payload: &SmallbankTransactionPayload) -> Vec<String> {
    match payload.get_payload_type() {
        SBPayloadType::CREATE_ACCOUNT =>
            vec![customer_id_address(payload.get_create_account().get_customer_id())],
        SBPayloadType::DEPOSIT_CHECKING =>
            vec![customer_id_address(payload.get_deposit_checking().get_customer_id())],
        SBPayloadType::WRITE_CHECK =>
            vec![customer_id_address(payload.get_write_check().get_customer_id())],
        SBPayloadType::TRANSACT_SAVINGS =>
            vec![customer_id_address(payload.get_transact_savings().get_customer_id())],
        SBPayloadType::SEND_PAYMENT =>
            vec![customer_id_address(payload.get_send_payment().get_source_customer_id()),
                 customer_id_address(payload.get_send_payment().get_dest_customer_id())],
        SBPayloadType::AMALGAMATE=>
            vec![customer_id_address(payload.get_amalgamate().get_source_customer_id()),
                 customer_id_address(payload.get_amalgamate().get_dest_customer_id())],
    }
}

fn customer_id_address(customer_id: u32) -> String {
    let mut sha = Sha512::new();
    sha.input(customer_id.to_string().as_bytes());
    let mut hash: &mut [u8] = & mut [0; 64];
    sha.result(hash);

    let hex = bytes_to_hex_str(hash);
    // Using the precomputed Sha512 hash of "supplychain"
    return  String::from("332514") + &hex[0..32];
}

pub fn create_smallbank_playlist(num_accounts: usize,
                                 num_transactions: usize,
                                 seed: Option<i32>)
    -> Box<Iterator<Item=SmallbankTransactionPayload>>
{
    let rng = match seed {
        Some(seed) => {
            let v = vec![seed as usize];
            let seed: &[usize] =  &v;
            SeedableRng::from_seed(seed)
        },
        None => StdRng::new().unwrap()
    };
    Box::new(SmallbankGeneratingIter {
        num_accounts: num_accounts,
        current_account: 0,
        num_transactions: num_transactions,
        current_transaction: 0,
        rng: rng
    })
}

pub fn read_smallbank_playlist<'a>(input: &'a mut Read)
    -> Result<Vec<SmallbankTransactionPayload>, PlaylistError>
{
    let mut results = Vec::new();
    let buf = try!(read_yaml(input));
    let yaml_array = try!(load_yaml_array(buf));
    for yaml in yaml_array.iter() {
        results.push(SmallbankTransactionPayload::from(yaml));
    }

    Ok(results)
}

fn read_yaml<'a>(input: &'a mut Read) -> Result<Cow<'a, str>, PlaylistError> {
   let mut buf: String = String::new();
   try!(input.read_to_string(&mut buf).map_err(PlaylistError::IoError));
   Ok(buf.into())
}

fn load_yaml_array<'a>(yaml_str: Cow<'a, str>) -> Result<Cow<'a, Vec<Yaml>>, PlaylistError> {
    let mut yaml = try!(YamlLoader::load_from_str(yaml_str.as_ref()).map_err(PlaylistError::YamlInputError));
    let element = yaml.remove(0);
    let yaml_array = element.as_vec().cloned().unwrap().clone();

    Ok(Cow::Owned(yaml_array))
}


struct SmallbankGeneratingIter {
    num_accounts: usize,
    current_account: usize,
    num_transactions: usize,
    current_transaction: usize,
    rng: StdRng,
}

impl Iterator for SmallbankGeneratingIter {
    type Item = SmallbankTransactionPayload;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_account < self.num_accounts {
            let mut payload =  SmallbankTransactionPayload::new();

            let mut create_account = smallbank::SmallbankTransactionPayload_CreateAccountTransactionData::new();
            create_account.set_customer_id(self.current_account as u32);
            create_account.set_customer_name(format!("customer_{:06}", self.current_account));

            create_account.set_initial_savings_balance(1000000);
            create_account.set_initial_checking_balance(1000000);
            payload.set_create_account(create_account);

            self.current_account += 1;

            Some(payload)
        } else if self.current_transaction < self.num_transactions {
            let mut payload =  SmallbankTransactionPayload::new();

            let payload_type = match self.rng.gen_range(2, 7) {
                2 => SBPayloadType::DEPOSIT_CHECKING,
                3 => SBPayloadType::WRITE_CHECK,
                4 => SBPayloadType::TRANSACT_SAVINGS,
                5 => SBPayloadType::SEND_PAYMENT,
                6 => SBPayloadType::AMALGAMATE,
                _ => panic!("Should not have generated outside of [2, 7)")
            };

            payload.set_payload_type(payload_type);

            match payload_type {
                SBPayloadType::DEPOSIT_CHECKING => {
                    let data = make_smallbank_deposit_checking_txn(&mut self.rng, self.num_accounts);
                    payload.set_deposit_checking(data);
                },
                SBPayloadType::WRITE_CHECK => {
                    let data = make_smallbank_write_check_txn(&mut self.rng, self.num_accounts);
                    payload.set_write_check(data);
                },
                SBPayloadType::TRANSACT_SAVINGS => {
                    let data = make_smallbank_transact_savings_txn(&mut self.rng, self.num_accounts);
                    payload.set_transact_savings(data);
                },
                SBPayloadType::SEND_PAYMENT => {
                    let data = make_smallbank_send_payment_txn(&mut self.rng, self.num_accounts);
                    payload.set_send_payment(data);
                },
                SBPayloadType::AMALGAMATE => {
                    let data = make_smallbank_amalgamate_txn(&mut self.rng, self.num_accounts);
                    payload.set_amalgamate(data);
                },
                _ => panic!("Should not have generated outside of [2, 7)")
            };

            self.current_transaction += 1;

            Some(payload)
        } else {
            None
        }
    }
}

impl From<SmallbankTransactionPayload> for Yaml {
    fn from(payload: SmallbankTransactionPayload) -> Self {

        match payload.payload_type {
            SBPayloadType::CREATE_ACCOUNT => {
                let data = payload.get_create_account();
                yaml_map!{
                    "transaction_type" => Yaml::from_str("create_account"),
                    "customer_id" => Yaml::Integer(data.customer_id as i64),
                    "customer_name" => Yaml::String(data.customer_name.clone()),
                    "initial_savings_balance" =>
                        Yaml::Integer(data.initial_savings_balance as i64),
                    "initial_checking_balance" =>
                        Yaml::Integer(data.initial_checking_balance as i64)}
            },
            SBPayloadType::DEPOSIT_CHECKING => {
                let data = payload.get_deposit_checking();
                yaml_map!{
                    "transaction_type" => Yaml::from_str("deposit_checking"),
                    "customer_id" => Yaml::Integer(data.customer_id as i64),
                    "amount" => Yaml::Integer(data.amount as i64)}
            },
            SBPayloadType::WRITE_CHECK => {
                let data  = payload.get_write_check();
                yaml_map!{
                    "transaction_type" => Yaml::from_str("write_check"),
                    "customer_id" => Yaml::Integer(data.customer_id as i64),
                    "amount" => Yaml::Integer(data.amount as i64)}
            },
            SBPayloadType::TRANSACT_SAVINGS => {
                let data = payload.get_transact_savings();
                yaml_map!{
                    "transaction_type" => Yaml::from_str("transact_savings"),
                    "customer_id" => Yaml::Integer(data.customer_id as i64),
                    "amount" => Yaml::Integer(data.amount as i64)}
            },
            SBPayloadType::SEND_PAYMENT => {
                let data = payload.get_send_payment();
                yaml_map!{
                    "transaction_type" => Yaml::from_str("send_payment"),
                    "source_customer_id" => Yaml::Integer(data.source_customer_id as i64),
                    "dest_customer_id" => Yaml::Integer(data.dest_customer_id as i64),
                    "amount" => Yaml::Integer(data.amount as i64)}
            },
            SBPayloadType::AMALGAMATE => {
                let data = payload.get_amalgamate();
                yaml_map!{
                    "transaction_type" => Yaml::from_str("amalgamate"),
                    "source_customer_id" => Yaml::Integer(data.source_customer_id as i64),
                    "dest_customer_id" => Yaml::Integer(data.dest_customer_id as i64)}
            },
        }
    }
}

impl<'a> From<&'a Yaml> for SmallbankTransactionPayload {
    fn from(yaml: &Yaml) -> Self {
        if let Some(txn_hash) = yaml.as_hash() {
            let mut payload = SmallbankTransactionPayload::new();
            match txn_hash[&Yaml::from_str("transaction_type")].as_str() {
                Some("create_account") => {
                    payload.set_payload_type(SBPayloadType::CREATE_ACCOUNT);
                    let mut data = smallbank::SmallbankTransactionPayload_CreateAccountTransactionData::new();
                    data.set_customer_id(txn_hash[&Yaml::from_str("customer_id")].as_i64().unwrap() as u32);
                    data.set_customer_name(txn_hash[&Yaml::from_str("customer_name")].as_str().unwrap().to_string());
                    data.set_initial_savings_balance(
                        txn_hash[&Yaml::from_str("initial_savings_balance")].as_i64().unwrap() as u32);
                    data.set_initial_checking_balance(
                        txn_hash[&Yaml::from_str("initial_checking_balance")].as_i64().unwrap() as u32);
                    payload.set_create_account(data);
                },

                Some("deposit_checking") => {
                    payload.set_payload_type(SBPayloadType::DEPOSIT_CHECKING);
                    let mut data = smallbank::SmallbankTransactionPayload_DepositCheckingTransactionData::new();
                    data.set_customer_id(
                        txn_hash[&Yaml::from_str("customer_id")].as_i64().unwrap() as u32);
                    data.set_amount(
                        txn_hash[&Yaml::from_str("amount")].as_i64().unwrap() as u32);
                    payload.set_deposit_checking(data);
                },

                Some("write_check") => {
                    payload.set_payload_type(SBPayloadType::WRITE_CHECK);
                    let mut data = smallbank::SmallbankTransactionPayload_WriteCheckTransactionData::new();
                    data.set_customer_id(
                        txn_hash[&Yaml::from_str("customer_id")].as_i64().unwrap() as u32);
                    data.set_amount(
                        txn_hash[&Yaml::from_str("amount")].as_i64().unwrap() as u32);
                    payload.set_write_check(data);
                },

                Some("transact_savings") => {
                    payload.set_payload_type(SBPayloadType::TRANSACT_SAVINGS);
                    let mut data = smallbank::SmallbankTransactionPayload_TransactSavingsTransactionData::new();
                    data.set_customer_id(
                        txn_hash[&Yaml::from_str("customer_id")].as_i64().unwrap() as u32);
                    data.set_amount(
                        txn_hash[&Yaml::from_str("amount")].as_i64().unwrap() as i32);
                    payload.set_transact_savings(data);
                },

                Some("send_payment") => {
                    payload.set_payload_type(SBPayloadType::SEND_PAYMENT);
                    let mut data = smallbank::SmallbankTransactionPayload_SendPaymentTransactionData::new();
                    data.set_source_customer_id(
                        txn_hash[&Yaml::from_str("source_customer_id")].as_i64().unwrap() as u32);
                    data.set_dest_customer_id(
                        txn_hash[&Yaml::from_str("dest_customer_id")].as_i64().unwrap() as u32);
                    data.set_amount(
                        txn_hash[&Yaml::from_str("amount")].as_i64().unwrap() as u32);
                    payload.set_send_payment(data);
                },

                Some("amalgamate") => {
                    payload.set_payload_type(SBPayloadType::AMALGAMATE);
                    let mut data = smallbank::SmallbankTransactionPayload_AmalgamateTransactionData::new();
                    data.set_source_customer_id(
                        txn_hash[&Yaml::from_str("source_customer_id")].as_i64().unwrap() as u32);
                    data.set_dest_customer_id(
                        txn_hash[&Yaml::from_str("dest_customer_id")].as_i64().unwrap() as u32);
                    payload.set_amalgamate(data);
                },
                Some(txn_type) => panic!(format!("unknown transaction_type: {}", txn_type)),
                None => panic!("No transaction_type specified"),
            }
            payload
        }
        else {
            panic!("should be a hash map!")
        }

    }
}

fn make_smallbank_deposit_checking_txn(rng: &mut StdRng, num_accounts: usize)
    -> smallbank::SmallbankTransactionPayload_DepositCheckingTransactionData
{
    let mut payload =
        smallbank::SmallbankTransactionPayload_DepositCheckingTransactionData::new();
    payload.set_customer_id(rng.gen_range(0, num_accounts as u32));
    payload.set_amount(rng.gen_range(10, 200));

    payload
}

fn make_smallbank_write_check_txn(rng: &mut StdRng, num_accounts: usize)
    -> smallbank::SmallbankTransactionPayload_WriteCheckTransactionData
{
    let mut payload =
        smallbank::SmallbankTransactionPayload_WriteCheckTransactionData::new();
    payload.set_customer_id(rng.gen_range(0, num_accounts as u32));
    payload.set_amount(rng.gen_range(10, 200));

    payload
}

fn make_smallbank_transact_savings_txn(rng: &mut StdRng, num_accounts: usize)
    -> smallbank::SmallbankTransactionPayload_TransactSavingsTransactionData
{
    let mut payload =
        smallbank::SmallbankTransactionPayload_TransactSavingsTransactionData::new();
    payload.set_customer_id(rng.gen_range(0, num_accounts as u32));
    payload.set_amount(rng.gen_range(10, 200));

    payload
}

fn make_smallbank_send_payment_txn(rng: &mut StdRng, num_accounts: usize)
    -> smallbank::SmallbankTransactionPayload_SendPaymentTransactionData
{
    let mut payload =
        smallbank::SmallbankTransactionPayload_SendPaymentTransactionData::new();
    let source_id = rng.gen_range(0, num_accounts as u32);
    let dest_id = next_non_matching_in_range(rng, num_accounts as u32, source_id);
    payload.set_source_customer_id(source_id);
    payload.set_dest_customer_id(dest_id);
    payload.set_amount(rng.gen_range(10, 200));

    payload
}

fn make_smallbank_amalgamate_txn(rng: &mut StdRng, num_accounts: usize)
    -> smallbank::SmallbankTransactionPayload_AmalgamateTransactionData
{
    let mut payload =
        smallbank::SmallbankTransactionPayload_AmalgamateTransactionData::new();
    let source_id = rng.gen_range(0, num_accounts as u32);
    let dest_id = next_non_matching_in_range(rng, num_accounts as u32, source_id);
    payload.set_source_customer_id(source_id);
    payload.set_dest_customer_id(dest_id);

    payload
}

fn next_non_matching_in_range(rng: &mut StdRng, max: u32, exclude: u32) -> u32 {
    let mut selected = exclude;
    while selected == exclude {
        selected = rng.gen_range(0, max)
    }
    selected
}

#[derive(Debug)]
pub enum PlaylistError {
    IoError(StdIoError),
    YamlOutputError(EmitError),
    YamlInputError(yaml_rust::ScanError),
    MessageError(protobuf::ProtobufError),
    SigningError(signing::Error),
}

impl fmt::Display for PlaylistError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            PlaylistError::IoError(ref err) =>
                write!(f, "Error occurred writing messages: {}", err),
            PlaylistError::YamlOutputError(_) =>
                write!(f, "Error occurred generating YAML output"),
            PlaylistError::YamlInputError(_) =>
                write!(f, "Error occurred reading YAML input"),
            PlaylistError::MessageError(ref err) =>
                write!(f, "Error occurred creating protobuf: {}", err),
            PlaylistError::SigningError(ref err) =>
                write!(f, "Error occurred signing transactions: {}", err),
        }
    }
}

impl error::Error for PlaylistError {
    fn description(&self) -> &str {
        match *self {
            PlaylistError::IoError(ref err) => err.description(),
            PlaylistError::YamlOutputError(_) => "Yaml Output Error",
            PlaylistError::YamlInputError(_) => "Yaml Input Error",
            PlaylistError::MessageError(ref err) => err.description(),
            PlaylistError::SigningError(ref err) => err.description(),
        }
    }

    fn cause(&self) -> Option<&error::Error> {
        match *self {
            PlaylistError::IoError(ref err) => Some(err),
            PlaylistError::YamlOutputError(_) => None,
            PlaylistError::YamlInputError(_) => None,
            PlaylistError::MessageError(ref err) => Some(err),
            PlaylistError::SigningError(ref err) => Some(err),
        }
    }
}


struct FmtWriter<'a> {
    writer: Box<&'a mut Write>
}

impl<'a> FmtWriter<'a> {
    pub fn new(writer: &'a mut Write) -> Self {
        FmtWriter {
            writer: Box::new(writer)
        }
    }
}

impl<'a> fmt::Write for FmtWriter<'a> {
    fn write_str(&mut self, s: &str) -> Result<(), fmt::Error> {
        let mut w = &mut *self.writer;
        w.write_all(s.as_bytes()).map_err(|_| fmt::Error::default())
    }
}

fn bytes_to_hex_str(b: &[u8]) -> String {
    b.iter()
     .map(|b| format!("{:02x}", b))
     .collect::<Vec<_>>()
     .join("")
}
