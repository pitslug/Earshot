//! JNI bridge receiving PCM from the Android Kotlin foreground service.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "earshot-capture-android");
    }
}
