#![cfg_attr(not(feature = "std"), no_std)]

// TODO: we need to figure out the key function here
// which is how to convert JsonValue::number to PrimitiveOracleType

use frame_support::{
	debug, decl_error, decl_event, decl_module, decl_storage, dispatch, traits::Get,
	IterableStorageDoubleMap, IterableStorageMap,
};
use frame_system::{
	ensure_root, ensure_signed,
	offchain::{
		AppCrypto, CreateSignedTransaction, SendSignedTransaction, SendUnsignedTransaction,
		SignedPayload, Signer, SigningTypes, SubmitTransaction,
	},
};
use sp_runtime::{
	FixedU128, offchain::{http, storage::StorageValueRef, Duration},
	transaction_validity::{
		InvalidTransaction, TransactionPriority, TransactionSource, TransactionValidity,
		ValidTransaction,
	},
};
use sp_core::crypto::KeyTypeId;
use codec::{Decode, Encode};
use lite_json::json::{JsonValue, NumberValue};
use sp_runtime::{
	curve::PiecewiseLinear,
	traits::{
		AtLeast32BitUnsigned, CheckedSub, Convert, Dispatchable, SaturatedConversion, Saturating,
		StaticLookup, Zero,
	},
	DispatchError, DispatchResult, PerThing, PerU16, Percent, Permill, RuntimeDebug,
};
use sp_std::{prelude::*, str::Chars, convert::TryInto};


pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"orcl");

pub mod crypto {
	use super::KEY_TYPE;
	use sp_runtime::{
		app_crypto::{app_crypto, sr25519},
		traits::Verify,
	};
	use sp_core::sr25519::Signature as Sr25519Signature;
	app_crypto!(sr25519, KEY_TYPE);

	pub struct TestAuthId;
	impl frame_system::offchain::AppCrypto<<Sr25519Signature as Verify>::Signer, Sr25519Signature> for TestAuthId {
		type RuntimeAppPublic = Public;
		type GenericSignature = sp_core::sr25519::Signature;
		type GenericPublic = sp_core::sr25519::Public;
	}
}


#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Encode, Decode, RuntimeDebug)]
pub enum Operations {
	Average,
	Sum,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Encode, Decode, RuntimeDebug)]
pub enum PrimitiveOracleType {
	U8(u8),
	U16(u16),
	U32(u32),
	FixedU128(FixedU128),
}

impl PrimitiveOracleType {
	pub fn number_type(&self) -> NumberType {
		match self {
			PrimitiveOracleType::U8(_) => NumberType::U8,
			PrimitiveOracleType::U16(_) => NumberType::U16,
			PrimitiveOracleType::U32(_) => NumberType::U32,
			PrimitiveOracleType::FixedU128(_) => NumberType::FixedU128,
		}
	}

	pub fn from_number_value(val: NumberValue, target_type: NumberType) -> Option<Self> {
		match target_type {
			// TODO: handle overflow/underflow error here
			NumberType::U8 => Some(PrimitiveOracleType::U8(val.integer.try_into().unwrap())),
			NumberType::U16 => Some(PrimitiveOracleType::U16(val.integer.try_into().unwrap())),
			NumberType::U32 => Some(PrimitiveOracleType::U32(val.integer.try_into().unwrap())),
			NumberType::FixedU128 => {
				Some(PrimitiveOracleType::FixedU128(FixedU128::from(
					(val.integer * 10i64.pow(val.fraction_length + val.exponent as u32) + (val.fraction as i64).pow(val.exponent as u32),
					 10i64.pow(val.fraction_length))
				)))
			}
		}
	}
}


#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Encode, Decode, RuntimeDebug)]
pub enum NumberType {
	U8,
	U16,
	U32,
	FixedU128,
}

impl Default for PrimitiveOracleType {
	fn default() -> Self {
		PrimitiveOracleType::U8(0)
	}
}

type StorageKey = Vec<u8>;

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode, RuntimeDebug)]
pub struct Info<BlockNumber> {
	/// the key of what we want from the json callback
	key_str: Vec<u8>,
	number_type: NumberType,
	operation: Operations,
	schedule: BlockNumber,
}

impl<BlockNumber> Info<BlockNumber> {
	fn key_str(&self) -> &[u8] {
		&self.key_str
	}

	fn number_type(&self) -> NumberType {
		self.number_type
	}
}


const RING_BUF_LEN: usize = 8;

use frame_support::traits::{Contains, EnsureOrigin};
use sp_std::{marker::PhantomData, prelude::*};

pub struct DataFeedGet<Key: Get<[u8; 32]>, Value: Get<PrimitiveOracleType>>(
	PhantomData<Key>,
	PhantomData<Value>,
);

impl<Key: Get<[u8; 32]>, Value: Get<PrimitiveOracleType>> Get<PrimitiveOracleType>
for DataFeedGet<Key, Value>
{
	fn get() -> PrimitiveOracleType {
		frame_support::storage::unhashed::get_or(Key::get().as_ref(), Value::get())
	}
}

pub trait Trait: CreateSignedTransaction<Call<Self>> {
	/// The identifier type for an offchain worker.
	type AuthorityId: AppCrypto<Self::Public, Self::Signature>;
	/// Because this pallet emits events, it depends on the runtime's definition of an event.
	type Event: From<Event<Self>> + Into<<Self as frame_system::Trait>::Event>;
	/// The overarching dispatch call type.
	type Call: From<Call<Self>>;
	/// The origin that can schedule an update
	type DispatchOrigin: EnsureOrigin<Self::Origin, Success=Self::AccountId>;
}

decl_event!(
    pub enum Event<T>
    where
        AccountId = <T as frame_system::Trait>::AccountId,
    {
        SomethingStored(u32, AccountId),
    }
);

// Errors inform users that something went wrong.
decl_error! {
    pub enum Error for Module<T: Trait> {
        ExistedKey,
        InvalidKey,
        InvalidValue,
    }
}

decl_storage! {
    // A unique name is used to ensure that the pallet's storage items are isolated.
    // This name may be updated, but each pallet in the runtime must use a unique name.
    // ---------------------------------vvvvvvvvvvvvvv
    trait Store for Module<T: Trait> as Oracle {
        pub ActiveParamTypes get(fn all_types): Vec<StorageKey>;

        pub ActiveProviders get(fn all_providers): Vec<T::AccountId>;

        // pub LastUpdated get(fn last_updated): hasher(twox_64_concat) StorageKey => T::BlockNumebr;

        pub Infos get(fn infos): map hasher(twox_64_concat) StorageKey => Option<Info<T::BlockNumber>>;

        /// permissioned URL that could be used to fetch data
        pub Url get(fn url): map hasher(twox_64_concat) StorageKey => Option<Vec<u8>>;

        pub DataFeeds get(fn member_score): double_map hasher(blake2_128_concat) StorageKey,
            hasher(blake2_128_concat) T::AccountId => Option<[PrimitiveOracleType; RING_BUF_LEN]>;
    }
}

impl<T: Trait> Contains<T::AccountId> for Module<T> {
	fn sorted_members() -> Vec<T::AccountId> {
		Self::all_providers()
	}
}

decl_module! {
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        // Events must be initialized if they are used by the pallet.
        fn deposit_event() = default;

        #[weight = 0]
        pub fn register_storage_key(origin, key: StorageKey, info: Info<T::BlockNumber>) -> DispatchResult {
            T::DispatchOrigin::try_origin(origin).map(|_| ()).or_else(ensure_root)?;
            // parse origin
            ActiveParamTypes::try_mutate::<_, DispatchError, _>(|v| {
                if v.contains(&key) {
                    return Err(Error::<T>::ExistedKey)?;
                }
                v.push(key.clone());
                Ok(())
            })?;
            Infos::<T>::insert(key, info);
            Ok(())
        }

        #[weight = 0]
        pub fn remove_storage_key(origin, key: StorageKey) -> DispatchResult {
            T::DispatchOrigin::try_origin(origin).map(|_| ()).or_else(ensure_root)?;
            ActiveParamTypes::mutate(|v| {
                // remove key
                v.retain(|k| k != &key);
            });
            Infos::<T>::remove(&key);
            DataFeeds::<T>::remove_prefix(&key);
            Ok(())
        }

        #[weight = 0]
        pub fn feed_data(origin, key: StorageKey, value: PrimitiveOracleType) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::feed_data_impl(who, key, value)
        }

        #[weight = 0]
        pub fn add_provider(origin, new_one: <T::Lookup as StaticLookup>::Source) -> DispatchResult {
            T::DispatchOrigin::try_origin(origin).map(|_| ()).or_else(ensure_root)?;
            let new_one = T::Lookup::lookup(new_one)?;

            ActiveProviders::<T>::mutate(|v| {
                if !v.contains(&new_one) {
                    v.push(new_one);
                }
            });
            Ok(())
        }

        #[weight = 0]
        pub fn remove_provider(origin, who: <T::Lookup as StaticLookup>::Source) -> DispatchResult {
            T::DispatchOrigin::try_origin(origin).map(|_| ()).or_else(ensure_root)?;
            let who = T::Lookup::lookup(who)?;

            ActiveProviders::<T>::mutate(|v| {
                v.retain(|accountid| accountid != &who);
            });
            Ok(())
        }

        fn on_finalize(_n: T::BlockNumber) {
            for (key, info) in Infos::<T>::iter() {
                if info.schedule % _n == Zero::zero() {
                    Self::calc(key, info);
                }
            }
        }

        fn offchain_worker(block_number: T::BlockNumber) {
            debug::native::info!("Hello World from offchain workers!");

            let res = Self::fetch_datas();
            if let Err(e) = res {
                debug::error!("Error: {}", e);
            }
        }
    }
}
impl<T: Trait> Module<T> {
	fn feed_data_impl(
		who: T::AccountId,
		key: StorageKey,
		value: PrimitiveOracleType,
	) -> DispatchResult {
		if !Self::all_types().contains(&key) {
			Err(Error::<T>::InvalidKey)?
		}
		let info: Info<_> = Self::infos(&key).ok_or(Error::<T>::InvalidKey)?;
		if value.number_type() != info.number_type {
			Err(Error::<T>::InvalidValue)?
		}
		DataFeeds::<T>::try_mutate_exists(key, who, |data| {
			match data {
				Some(data) => {
					let mut new_data = [Default::default(); RING_BUF_LEN];
					new_data[0] = value;
					// move data buf to new_data buf, and drop last item
					new_data[1..].copy_from_slice(&data[0..(data.len() - 2)]);
					*data = new_data;
				}
				None => {
					*data = Some([value; RING_BUF_LEN]);
				}
			}
			Ok(())
		})
	}

	fn calc(key: StorageKey, info: Info<T::BlockNumber>) {
		for (account_id, datas) in DataFeeds::<T>::drain_prefix(&key) {
			match info.operation {
				Operations::Sum => {
					// TODO
					PrimitiveOracleType::default()
				}
				Operations::Average => {
					// TODO
					PrimitiveOracleType::default()
				}
			};
		}
		// TODO
		let value = PrimitiveOracleType::default();
		Self::set_storage_value(key, value);
	}

	fn set_storage_value(key: Vec<u8>, value: PrimitiveOracleType) {
		frame_support::storage::unhashed::put(&key, &value);
	}

	fn fetch_datas() -> Result<(), &'static str> {
		// todo get storage keys and call fetch_data
		Ok(())
	}

	fn fetch_data(storage_key: &StorageKey) -> Result<PrimitiveOracleType, &'static str> {
		// We want to keep the offchain worker execution time reasonable, so we set a hard-coded
		// deadline to 2s to complete the external call.
		// You can also wait idefinitely for the response, however you may still get a timeout
		// coming from the host machine.
		let deadline = sp_io::offchain::timestamp().add(Duration::from_millis(2_000));
		// TODO: if error
		let url = Self::url(storage_key).ok_or("no url for this storage key")?;

		let request =
			http::Request::get(core::str::from_utf8(&url).map_err(|_| "parse url error")?);

		// to alter request headers or stream body content in case of non-GET requests.
		let pending = request
			.deadline(deadline)
			.send()
			.map_err(|_| "Request had timed out.")?;

		let response = pending
			.try_wait(deadline)
			.map_err(|_| "Deadline has been reached")?
			.map_err(|_| "http error")?;
		// Let's check the status code before we proceed to reading the response.
		if response.code != 200 {
			debug::warn!("Unexpected status code: {}", response.code);
			Err("Unknown error has been encountered.")?;
		}

		let body = response.body().collect::<Vec<u8>>();
		// TODO: handle error here
		let info = Self::infos(&storage_key).ok_or("storage key not exist")?;
		let number_type = info.number_type();

		// Create a str slice from the body.
		let body_str = sp_std::str::from_utf8(&body).map_err(|_| {
			debug::warn!("No UTF8 body");
			"Unknown error has been encountered."
		})?;
		// TODO: handle error
		let key_str = core::str::from_utf8(info.key_str())
			.map_err(|_| "json key is invalid")?
			.chars();
		let data = match Self::parse_data(key_str, number_type, body_str) {
			Some(data) => Ok(data),
			None => {
				debug::warn!("Unable to extract price from the response: {:?}", body_str);
				Err("Unknown error has been encountered.")
			}
		}?;
		//		debug::warn!("Got Data: {}", data);
		Ok(data)
	}

	fn parse_data(key_str: Chars, number_type: NumberType, data: &str) -> Option<PrimitiveOracleType> {
		let mut key_str = key_str;
		let val = lite_json::parse_json(data);
		let output = val.ok().and_then(|v| match v {
			JsonValue::Object(obj) => obj
				.into_iter()
				.find(|(k, _)| k.iter().all(|k| Some(*k) == key_str.next()))
				.and_then(|v| match v.1 {
					JsonValue::Number(number) => Some(number),
					_ => None,
				}),
			_ => None,
		})?;

		PrimitiveOracleType::from_number_value(output, number_type)
	}
}
