#[repr(u8)]
#[derive(Default)]
pub(crate) enum RemoteController {
    #[default]
    None = 0,
    Usb = 1,
    Virtual = 2,
}