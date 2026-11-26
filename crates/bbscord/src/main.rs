//! `bbscord` binary entrypoint stub — wired in Wave 2.

fn main() -> anyhow::Result<()> {
    let _ = bbs_core::CRATE_VERSION;
    println!("bbscord {} (Wave 0 workspace bootstrap)", bbs_core::CRATE_VERSION);
    Ok(())
}
