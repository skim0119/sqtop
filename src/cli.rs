use clap::Parser;

#[derive(Parser)]
#[command(version, about)]
pub struct Args {
    /// SSH alias or user@host (uses your OpenSSH configuration)
    #[arg(value_name = "SSH_HOST", value_parser = ssh_host)]
    pub host: String,
}

fn ssh_host(value: &str) -> Result<String, String> {
    if value.is_empty() || value.starts_with('-') || value.contains(char::is_whitespace) {
        return Err("provide a valid SSH alias or user@host".into());
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ssh_destinations() {
        for host in ["frontera", "alice@login.example.com", "alice@192.0.2.1"] {
            assert_eq!(Args::try_parse_from(["sqtop", host]).unwrap().host, host);
        }
    }

    #[test]
    fn rejects_invalid_destinations() {
        assert!(Args::try_parse_from(["sqtop"]).is_err());
        assert!(Args::try_parse_from(["sqtop", "cluster", "extra"]).is_err());
        for host in ["", "-oProxyCommand=bad", "cluster name", "cluster\n"] {
            assert!(Args::try_parse_from(["sqtop", "--", host]).is_err());
        }
    }
}
