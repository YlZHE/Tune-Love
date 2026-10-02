pub fn parse_native_opt(value: Option<&str>) -> Result<Option<u32>, &'static str> {
    match value {
        None => Ok(None),
        Some("0") => Ok(Some(0)),
        Some("2") => Ok(Some(2)),
        _ => Err("KEYFINDER_NATIVE_OPT_LEVEL must be unset, 0, or 2"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_optimization_is_strict_and_opt_in() {
        assert_eq!(parse_native_opt(None).unwrap(), None);
        assert_eq!(parse_native_opt(Some("0")).unwrap(), Some(0));
        assert_eq!(parse_native_opt(Some("2")).unwrap(), Some(2));
        for bad in ["", "1", "3", " 2", "fast", "-1"] {
            assert!(parse_native_opt(Some(bad)).is_err());
        }
    }
}
