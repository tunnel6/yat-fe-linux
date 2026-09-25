use anyhow::Result;

use crate::managers::WgManager;

/// Show certificate information.
pub fn run() -> Result<()> {
  let wg = WgManager::new()?;

  match wg.certs().cert_info() {
    Ok(info) => {
      println!("Client certificate: {}", info.cert_path);
      println!("  Subject:     {}", info.subject);
      println!("  Issuer:      {}", info.issuer);
      println!("  Valid from:  {}", info.not_before);
      println!("  Expires:     {}", info.not_after);
      match info.days_remaining {
        Some(d) => println!("  Remaining:   {d} days"),
        None => println!("  Remaining:   (unknown)"),
      }
      println!("  Fingerprint: {}", info.fingerprint);
    }
    Err(e) => {
      println!("No certificate info: {e}");
    }
  }

  Ok(())
}
