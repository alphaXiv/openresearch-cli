//! Takes no args. Loads credentials; if absent, reports "Not logged in." and
//! returns. Otherwise clears local credentials and prints a confirmation.

use crate::config::{clear_credentials, load_credentials};
use crate::error::Result;

pub async fn run() -> Result<()> {
    let creds = load_credentials().await?;
    if creds.is_none() {
        println!("Not logged in.");
        return Ok(());
    }
    clear_credentials().await?;
    println!("\u{2713} Logged out. Local credentials removed.");
    Ok(())
}
