use std::env;

use uuid::Uuid;

#[derive(Debug)]
pub(crate) struct Args {
    pub(crate) base_url: Option<String>,
    pub(crate) account_id: Option<Uuid>,
    pub(crate) token: Option<String>,
}

impl Args {
    pub(crate) fn parse() -> anyhow::Result<Self> {
        let mut base_url: Option<String> = env::var("AXON_BASE_URL").ok().filter(|s| !s.is_empty());
        let mut account_id = None;
        let mut token = normalize_token(env::var("AXON_TOKEN").ok());
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--base-url" => {
                    base_url = Some(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("--base-url requires a value"))?,
                    );
                }
                "--account-id" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--account-id requires a value"))?;
                    account_id = Some(value.parse()?);
                }
                "--token" => {
                    token = normalize_token(Some(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("--token requires a value"))?,
                    ));
                }
                "--version" | "-V" => {
                    println!("{}", version_line());
                    std::process::exit(0);
                }
                "--help" | "-h" => {
                    println!(
                        "Usage: axon-tui [--base-url URL] [--account-id UUID] [--token TOKEN] [--version]"
                    );
                    println!("  AXON_BASE_URL env var sets the default base URL (overrides built-in default).");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown argument: {other}"),
            }
        }
        Ok(Self {
            base_url,
            account_id,
            token,
        })
    }
}

fn version_line() -> String {
    format!("axon-tui {}", env!("CARGO_PKG_VERSION"))
}

pub(crate) fn normalize_token(token: Option<String>) -> Option<String> {
    token.and_then(|token| {
        let token = token.trim();
        (!token.is_empty()).then(|| token.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::{normalize_token, version_line};

    #[test]
    fn empty_tokens_are_absent() {
        assert_eq!(normalize_token(None), None);
        assert_eq!(normalize_token(Some(String::new())), None);
        assert_eq!(normalize_token(Some(" \t ".to_owned())), None);
        assert_eq!(
            normalize_token(Some("  secret-token  ".to_owned())).as_deref(),
            Some("secret-token")
        );
    }

    #[test]
    fn version_line_names_the_cargo_version() {
        let line = version_line();
        assert_eq!(
            line.strip_prefix("axon-tui "),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }
}
