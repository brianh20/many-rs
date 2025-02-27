use crate::{Identity, ManyError};
use many_macros::many_module;

#[cfg(test)]
use mockall::{automock, predicate::*};

pub mod get;
pub mod info;
pub use get::*;
pub use info::*;

#[many_module(name = KvStoreModule, id = 3, namespace = kvstore, many_crate = crate)]
#[cfg_attr(test, automock)]
pub trait KvStoreModuleBackend: Send {
    fn info(&self, sender: &Identity, args: InfoArg) -> Result<InfoReturns, ManyError>;
    fn get(&self, sender: &Identity, args: GetArgs) -> Result<GetReturns, ManyError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::module::testutils::{call_module, call_module_cbor};
    use crate::types::identity::testing::identity;
    use minicbor::bytes::ByteVec;
    use mockall::predicate;
    use std::sync::{Arc, Mutex};

    #[test]
    fn info() {
        let mut mock = MockKvStoreModuleBackend::new();
        mock.expect_info()
            .with(predicate::eq(identity(1)), predicate::eq(InfoArg {}))
            .times(1)
            .return_const(Ok(InfoReturns {
                hash: ByteVec::from(vec![9u8; 8]),
            }));
        let module = super::KvStoreModule::new(Arc::new(Mutex::new(mock)));
        let info_returns: InfoReturns =
            minicbor::decode(&call_module(1, &module, "kvstore.info", "null").unwrap()).unwrap();

        assert_eq!(info_returns.hash, ByteVec::from(vec![9u8; 8]));
    }

    #[test]
    fn get() {
        let data = GetArgs {
            key: ByteVec::from(vec![5, 6, 7]),
        };
        let mut mock = MockKvStoreModuleBackend::new();
        mock.expect_get()
        .with(predicate::eq(identity(1)), predicate::eq(data.clone()))
        .times(1).returning(|_id, _args| {
            Ok(GetReturns {
                value: Some(ByteVec::from(vec![1, 2, 3, 4])),
            })
        });
        let module = super::KvStoreModule::new(Arc::new(Mutex::new(mock)));

        let get_returns: GetReturns = minicbor::decode(
            &call_module_cbor(1, &module, "kvstore.get", minicbor::to_vec(data).unwrap()).unwrap(),
        )
        .unwrap();

        assert_eq!(get_returns.value, Some(ByteVec::from(vec![1, 2, 3, 4])));
    }
}
