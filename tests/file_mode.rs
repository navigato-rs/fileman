#[test]
fn file_type_bits_do_not_overlap() {
    assert_eq!(fileman::core::format_mode(0o100644), "-rw-r--r--");
    assert_eq!(fileman::core::format_mode(0o040755), "drwxr-xr-x");
    assert_eq!(fileman::core::format_mode(0o120777), "lrwxrwxrwx");
    assert_eq!(fileman::core::format_mode(0o000644), "-rw-r--r--");
}
