//! Development-only validator; does not change CLI verification or runtime loading.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().ok_or("expected dataset directory")?;
    let workspace: u64 = args
        .next()
        .ok_or("expected workspace bytes")?
        .to_str()
        .ok_or("workspace encoding")?
        .parse()?;
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    let manifest = routeweft_dataset::validate_binary_streaming(
        std::path::Path::new(&path),
        Default::default(),
        workspace,
    )?;
    println!("{}", serde_json::to_string(&manifest)?);
    Ok(())
}
