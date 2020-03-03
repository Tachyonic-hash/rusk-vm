use super::AbiCall;
use crate::host_fns::{ArgsExt, CallContext, Resolver};
use crate::VMError;

use kelvin::ByteHash;
use wasmi::{RuntimeArgs, RuntimeValue, ValueType};

pub struct SelfHash;

impl<S: Resolver<H>, H: ByteHash> AbiCall<S, H> for SelfHash {
    const ARGUMENTS: &'static [ValueType] = &[ValueType::I32];
    const RETURN: Option<ValueType> = None;

    fn call(
        context: &mut CallContext<S, H>,
        args: RuntimeArgs,
    ) -> Result<Option<RuntimeValue>, VMError> {
        let buffer_ofs = args.get(0)?;

        context.memory().with_direct_access_mut(|a| {
            a[buffer_ofs..buffer_ofs + 32]
                .copy_from_slice(context.called().as_ref())
        });
        Ok(None)
    }
}
