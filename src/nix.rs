pub(crate) trait ToNix {
    fn to_nix(&self) -> String;
}

impl ToNix for String {
    fn to_nix(&self) -> String {
        // TODO: replace " with \"
        format!(r#""{self}""#)
    }
}

impl ToNix for std::net::IpAddr {
    fn to_nix(&self) -> String {
        self.to_string().to_nix()
    }
}

impl ToNix for std::net::Ipv4Addr {
    fn to_nix(&self) -> String {
        self.to_string().to_nix()
    }
}

impl ToNix for std::net::Ipv6Addr {
    fn to_nix(&self) -> String {
        self.to_string().to_nix()
    }
}

impl ToNix for i32 {
    fn to_nix(&self) -> String {
        self.to_string()
    }
}

impl<T: ToNix> ToNix for Option<T> {
    fn to_nix(&self) -> String {
        match self {
            Some(val) => val.to_nix(),
            None => "null".to_string()
        }
    }
}
