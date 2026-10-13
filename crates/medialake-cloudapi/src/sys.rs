use windows::Win32::Storage::CloudFilters::*;
use windows::core::HSTRING;

pub fn probe() -> windows::core::Result<()> {
    let _ = unsafe { CfGetPlatformInfo()? };
    let _ = HSTRING::from("x");
    Ok(())
}
