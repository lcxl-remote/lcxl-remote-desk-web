//! Fixed COM primitives used only inside the isolated STA helper.
//! Names below are compile-time call sites, never accepted through the protocol.
use windows::{
    Win32::System::{
        Com::{
            DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET, DISPATCH_PROPERTYPUT,
            DISPPARAMS, IDispatch,
        },
        Variant::VARIANT,
    },
    core::{BSTR, GUID, PCWSTR},
};

pub struct Dispatch(pub IDispatch);
impl Dispatch {
    pub fn from_variant(value: &VARIANT) -> anyhow::Result<Self> {
        Ok(Self(IDispatch::try_from(value)?))
    }
    fn invoke(
        &self,
        name: &'static str,
        flags: DISPATCH_FLAGS,
        mut args: Vec<VARIANT>,
    ) -> anyhow::Result<VARIANT> {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut id = 0;
        unsafe {
            self.0
                .GetIDsOfNames(&GUID::zeroed(), &PCWSTR(wide.as_ptr()), 1, 0x409, &mut id)?;
        }
        args.reverse();
        let mut property_put = -3i32;
        let put = flags == DISPATCH_PROPERTYPUT;
        let params = DISPPARAMS {
            rgvarg: args.as_mut_ptr(),
            rgdispidNamedArgs: if put {
                &mut property_put
            } else {
                std::ptr::null_mut()
            },
            cArgs: args.len() as u32,
            cNamedArgs: u32::from(put),
        };
        let mut output = VARIANT::default();
        unsafe {
            self.0.Invoke(
                id,
                &GUID::zeroed(),
                0x409,
                flags,
                &params,
                Some(&mut output),
                None,
                None,
            )?;
        }
        Ok(output)
    }
    pub fn get(&self, name: &'static str) -> anyhow::Result<VARIANT> {
        self.invoke(name, DISPATCH_PROPERTYGET, vec![])
    }
    pub fn object(&self, name: &'static str, args: Vec<VARIANT>) -> anyhow::Result<Self> {
        Self::from_variant(&self.invoke(name, DISPATCH_PROPERTYGET, args)?)
    }
    pub fn call(&self, name: &'static str, args: Vec<VARIANT>) -> anyhow::Result<VARIANT> {
        self.invoke(name, DISPATCH_METHOD, args)
    }
    pub fn set(&self, name: &'static str, value: VARIANT) -> anyhow::Result<()> {
        self.invoke(name, DISPATCH_PROPERTYPUT, vec![value])
            .map(|_| ())
    }
    pub fn boolean(&self, name: &'static str) -> anyhow::Result<bool> {
        Ok(bool::try_from(&self.get(name)?)?)
    }
    pub fn integer(&self, name: &'static str) -> anyhow::Result<i32> {
        Ok(i32::try_from(&self.get(name)?)?)
    }
    pub fn text(&self, name: &'static str) -> anyhow::Result<String> {
        Ok(BSTR::try_from(&self.get(name)?)?.to_string())
    }
    pub fn set_boolean(&self, name: &'static str, value: bool) -> anyhow::Result<()> {
        self.set(name, value.into())?;
        anyhow::ensure!(
            self.boolean(name)? == value,
            "Excel did not retain required boolean policy"
        );
        Ok(())
    }
    pub fn set_integer(&self, name: &'static str, value: i32) -> anyhow::Result<()> {
        self.set(name, value.into())?;
        anyhow::ensure!(
            self.integer(name)? == value,
            "Excel did not retain required integer policy"
        );
        Ok(())
    }
}
