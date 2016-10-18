// Copyright 2015 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use core::{Client, CoreError, CoreFuture, SelfEncryptionStorage, utility};
use core::futures::FutureExt;
use futures::Future;
use maidsafe_utilities::serialisation::{deserialise, serialise};
use routing::{Data, DataIdentifier, ImmutableData, MAX_IMMUTABLE_DATA_SIZE_IN_BYTES, XorName};
use rust_sodium::crypto::secretbox;
use self_encryption::{DataMap, SelfEncryptor};

#[derive(RustcEncodable, RustcDecodable)]
enum DataTypeEncoding {
    Serialised(Vec<u8>),
    DataMap(DataMap),
}

/// Create and obtain immutable data out of the given raw bytes. The API will encrypt the right
/// content if the keys are provided and will ensure the maximum immutable data chunk size is
/// respected.
pub fn create(client: &Client,
              value: Vec<u8>,
              encryption_key: Option<secretbox::Key>)
              -> Box<CoreFuture<ImmutableData>> {
    trace!("Creating conformant ImmutableData.");

    let client = client.clone();
    let storage = SelfEncryptionStorage::new(client.clone());
    let self_encryptor = fry!(SelfEncryptor::new(storage, DataMap::None));

    self_encryptor.write(&value, 0)
        .and_then(move |_| self_encryptor.close())
        .map_err(From::from)
        .and_then(move |(data_map, _)| {
            let serialised_data_map = fry!(serialise(&data_map));

            let value = if let Some(key) = encryption_key {
                let cipher_text = fry!(utility::symmetric_encrypt(&serialised_data_map, &key));
                fry!(serialise(&DataTypeEncoding::Serialised(cipher_text)))
            } else {
                fry!(serialise(&DataTypeEncoding::Serialised(serialised_data_map)))
            };

            pack(client, value)
        })
        .into_box()
}

/// GET immutable data from the network.
pub fn get(client: &Client, name: &XorName) -> Box<CoreFuture<ImmutableData>> {
    let data_id = DataIdentifier::Immutable(*name);
    client.get(data_id, None)
        .and_then(|data| match data {
            Data::Immutable(data) => Ok(data),
            _ => Err(CoreError::ReceivedUnexpectedData),
        })
        .into_box()
}

/// Get the raw bytes from ImmutableData created via `create()` function in this module.
pub fn extract_value(client: &Client,
                     data: ImmutableData,
                     decryption_key: Option<secretbox::Key>)
                     -> Box<CoreFuture<Vec<u8>>> {
    let client = client.clone();

    unpack(client.clone(), data)
        .and_then(move |value| {
            let data_map = if let Some(key) = decryption_key {
                let plain_text =
                    try!(utility::symmetric_decrypt(&value, &key));
                try!(deserialise(&plain_text))
            } else {
                try!(deserialise(&value))
            };

            let storage = SelfEncryptionStorage::new(client);
            Ok(try!(SelfEncryptor::new(storage, data_map)))
        })
        .and_then(|self_encryptor| {
            let length = self_encryptor.len();
            self_encryptor.read(0, length).map_err(From::from)
        })
        .into_box()
}

/// Get immutable data from the network and extract its value, decrypting it in
/// the process (if keys provided).
/// This is a convenience function combining `get` and `extract_value` into one
/// function.
pub fn get_value(client: &Client,
                 name: &XorName,
                 decryption_key: Option<secretbox::Key>)
                 -> Box<CoreFuture<Vec<u8>>> {
    let client2 = client.clone();
    get(client, name)
        .and_then(move |data| extract_value(&client2, data, decryption_key))
        .into_box()
}

// TODO: consider rewriting these two function to not use recursion.

fn pack(client: Client, value: Vec<u8>) -> Box<CoreFuture<ImmutableData>> {
    let data = ImmutableData::new(value);
    let serialised_data = fry!(serialise(&data));

    if serialised_data.len() > MAX_IMMUTABLE_DATA_SIZE_IN_BYTES {
        let storage = SelfEncryptionStorage::new(client.clone());
        let self_encryptor = fry!(SelfEncryptor::new(storage, DataMap::None));
        self_encryptor.write(&serialised_data, 0)
            .and_then(move |_| self_encryptor.close())
            .map_err(From::from)
            .and_then(move |(data_map, _)| {
                let value = fry!(serialise(&DataTypeEncoding::DataMap(data_map)));
                pack(client, value)
            })
            .into_box()
    } else {
        ok!(data)
    }
}

fn unpack(client: Client, data: ImmutableData) -> Box<CoreFuture<Vec<u8>>> {
    match fry!(deserialise(data.value())) {
        DataTypeEncoding::Serialised(value) => ok!(value),
        DataTypeEncoding::DataMap(data_map) => {
            let storage = SelfEncryptionStorage::new(client.clone());
            let self_encryptor = fry!(SelfEncryptor::new(storage, data_map));
            let length = self_encryptor.len();
            self_encryptor.read(0, length)
                .map_err(From::from)
                .and_then(move |serialised_data| {
                    let data = fry!(deserialise(&serialised_data));
                    unpack(client, data)
                })
                .into_box()
        }
    }
}

#[cfg(test)]
mod tests {
    use core::utility;
    use core::utility::test_utils;
    use futures::Future;
    use routing::Data;
    use rust_sodium::crypto::secretbox;
    use super::*;

    #[test]
    fn create_and_retrieve_1kb() {
        create_and_retrieve(1024)
    }

    #[test]
    fn create_and_retrieve_1mb() {
        create_and_retrieve(1024 * 1024)
    }

    #[test]
    fn create_and_retrieve_2mb() {
        create_and_retrieve(2 * 1024 * 1024)
    }

    // #[ignore]'d becayse it takes a very long time in debug mode - it is due to S.E crate.
    #[test]
    #[ignore]
    fn create_and_retrieve_10mb() {
        create_and_retrieve(10 * 1024 * 1024)
    }

    fn create_and_retrieve(size: usize) {
        let value = unwrap!(utility::generate_random_vector(size));

        // Unencrypted
        {
            let value_before = value.clone();

            test_utils::register_and_run(move |client| {
                let client2 = client.clone();
                let client3 = client.clone();

                create(client, value_before.clone(), None)
                    .and_then(move |data_before| {
                        let data_name = *data_before.name();
                        client2.put(Data::Immutable(data_before), None)
                            .map(move |_| data_name)
                    })
                    .and_then(move |data_name| get_value(&client3, &data_name, None))
                    .map(move |value_after| {
                        assert_eq!(value_after, value_before);
                    })
                    .map_err(|error| panic!("Unexpected {:?}", error))
            })
        }

        // Encrypted
        {
            let value_before = value.clone();
            let key = secretbox::gen_key();

            test_utils::register_and_run(move |client| {
                let client2 = client.clone();
                let client3 = client.clone();

                create(client, value_before.clone(), Some(key.clone()))
                    .and_then(move |data_before| {
                        let data_name = *data_before.name();
                        client2.put(Data::Immutable(data_before), None)
                            .map(move |_| data_name)
                    })
                    .and_then(move |data_name| get_value(&client3, &data_name, Some(key)))
                    .map(move |value_after| {
                        assert_eq!(value_after, value_before);
                    })
                    .map_err(|error| panic!("Unexpected {:?}", error))
            })
        }

        // Put unencrypted Retrieve encrypted - Should fail
        {
            let value = value.clone();
            let key = secretbox::gen_key();

            test_utils::register_and_run(move |client| {
                let client2 = client.clone();
                let client3 = client.clone();

                create(client, value, None)
                    .and_then(move |data| {
                        let data_name = *data.name();
                        client2.put(Data::Immutable(data), None)
                            .map(move |_| data_name)
                    })
                    .and_then(move |data_name| get_value(&client3, &data_name, Some(key)))
                    .map(|_| panic!("get_value should fail"))
            })
        }

        // Put encrypted Retrieve unencrypted - Should fail
        {
            let value = value.clone();
            let key = secretbox::gen_key();

            test_utils::register_and_run(move |client| {
                let client2 = client.clone();
                let client3 = client.clone();

                create(client, value, Some(key))
                    .and_then(move |data| {
                        let data_name = *data.name();
                        client2.put(Data::Immutable(data), None)
                            .map(move |_| data_name)
                    })
                    .and_then(move |data_name| get_value(&client3, &data_name, None))
                    .map(|_| panic!("get_value should fail"))

            })
        }
    }
}
