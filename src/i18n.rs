use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_ulong};

const DOMAIN: &CStr = c"rarc";
const LC_ALL: c_int = 6;

unsafe extern "C" {
    fn setlocale(category: c_int, locale: *const c_char) -> *const c_char;
    fn bindtextdomain(domainname: *const c_char, dirname: *const c_char) -> *const c_char;
    fn bind_textdomain_codeset(domainname: *const c_char, codeset: *const c_char) -> *const c_char;
    fn textdomain(domainname: *const c_char) -> *const c_char;
    fn dgettext(domainname: *const c_char, msgid: *const c_char) -> *const c_char;
    fn dngettext(
        domainname: *const c_char,
        msgid1: *const c_char,
        msgid2: *const c_char,
        n: c_ulong,
    ) -> *const c_char;
}

/// Initializes process locale and binds the `rarc` gettext message catalog.
pub fn init() {
    let locale_dir = std::env::var("RARC_LOCALEDIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            option_env!("RARC_LOCALEDIR")
                .unwrap_or("/usr/share/locale")
                .to_string()
        });
    let Ok(c_dir) = CString::new(locale_dir) else {
        return;
    };
    unsafe {
        setlocale(LC_ALL, c"".as_ptr());
        bindtextdomain(DOMAIN.as_ptr(), c_dir.as_ptr());
        bind_textdomain_codeset(DOMAIN.as_ptr(), c"UTF-8".as_ptr());
        textdomain(DOMAIN.as_ptr());
    }
}

/// Translates `msgid` using the `rarc` gettext catalog.
pub fn tr(msgid: &str) -> String {
    let Ok(c_msg) = CString::new(msgid) else {
        return msgid.to_string();
    };
    let ptr = unsafe { dgettext(DOMAIN.as_ptr(), c_msg.as_ptr()) };
    if ptr.is_null() {
        msgid.to_string()
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Translates a singular/plural message pair using the `rarc` gettext catalog.
pub fn trn(singular: &str, plural: &str, n: u64) -> String {
    let (Ok(c_sing), Ok(c_plur)) = (CString::new(singular), CString::new(plural)) else {
        return if n == 1 { singular } else { plural }.to_string();
    };
    let ptr = unsafe { dngettext(DOMAIN.as_ptr(), c_sing.as_ptr(), c_plur.as_ptr(), n as c_ulong) };
    if ptr.is_null() {
        if n == 1 { singular } else { plural }.to_string()
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_returns_english_when_untranslated() {
        assert_eq!(tr("Open Archive"), "Open Archive");
        assert_eq!(trn("1 item", "{n} items", 1), "1 item");
        assert_eq!(trn("1 item", "{n} items", 5), "{n} items");
    }
}
