#![feature(proc_macro_hygiene)]
#![feature(result_map_or_else)]

pub mod schema;
pub mod models;
mod wireguard;

#[macro_use] extern crate diesel;
#[macro_use] extern crate itertools;
#[macro_use] extern crate runtime_fmt;

use std::io;
use std::iter;
use std::collections::{HashMap, HashSet};
use std::{env, path::PathBuf};
use std::net::{IpAddr, Ipv4Addr};
use std::io::Write;
use std::path::Path;
use std::fs;
use std::str;
use std::convert::TryFrom;
use tabwriter::{TabWriter, IntoInnerError};
use diesel::prelude::*;
use diesel::pg::PgConnection;
use dotenv;
use snafu::{ResultExt, Snafu, Backtrace, ErrorCompat};
use structopt::StructOpt;
use indoc::indoc;
use natural_sort::HumanStr;
use ipnetwork::IpNetwork;

use schema::{machines, network_links, providers};
use models::{Machine, NewMachine, MachineAddress, NetworkLink, Provider};

#[derive(Debug, Snafu)]
pub(crate) enum Error {
    #[snafu(display("Unable to read configuration from {}: {}", path.display(), source))]
    ReadConfiguration { source: dotenv::DotenvError, path: PathBuf },
    #[snafu(display("Could not find source machine {:?} in database", source_machine))]
    MissingSourceMachine { source_machine: String },
    Diesel { source: diesel::result::Error },
    DieselConnection { source: diesel::ConnectionError },
    #[snafu(display("Could not get variable {} from environment", var))]
    Var { source: env::VarError, var: String },
    Io { source: std::io::Error, backtrace: Backtrace },
    IntoInner { source: IntoInnerError<TabWriter<Vec<u8>>> },
    #[snafu(display("Could not parse variable {} as integer", var))]
    ParseInt { source: std::num::ParseIntError, var: String },
    #[snafu(display("Could not parse variable {} as IP address", var))]
    AddrParse { source: std::net::AddrParseError, var: String },
    #[snafu(display("Could not find an unused WireGuard IP address; check WIREGUARD_IP_START and WIREGUARD_IP_END"))]
    NoWireGuardAddressAvailable,
    NonZeroExit,
    NoStdin,
    FormatString,
    NoParentDirectory,
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io {
            source: err,
            backtrace: Backtrace::new(),
        }
    }
}

impl From<diesel::result::Error> for Error {
    fn from(source: diesel::result::Error) -> Self {
        Error::Diesel { source }
    }
}

type Result<T, E = Error> = std::result::Result<T, E>;

type DieselResult<T> = Result<T, diesel::result::Error>;

fn import_env() -> Result<()> {
    let path = dirs::config_dir().unwrap().join("infrabase").join("env");
    dotenv::from_path(&path).context(ReadConfiguration { path })
}

fn establish_connection() -> Result<PgConnection> {
    let database_url = env_var("DATABASE_URL")?;
    Ok(PgConnection::establish(&database_url).context(DieselConnection)?)
}

/// A map of (network, other_network) -> priority
type NetworkLinksMap = HashMap<(String, String), i32>;

fn get_network_links_map(connection: &PgConnection) -> DieselResult<NetworkLinksMap> {
    let map = network_links::table
        .load::<NetworkLink>(connection)?
        .into_iter()
        .map(|row| ((row.name, row.other_network), row.priority))
        .collect::<HashMap<_, _>>();
    Ok(map)
}

fn get_machines_and_addresses(connection: &PgConnection) -> DieselResult<Vec<(Machine, Vec<MachineAddress>)>> {
    connection.transaction::<_, _, _>(|| {
        let machines = machines::table
            .load::<Machine>(connection)?;

        let addresses = MachineAddress::belonging_to(&machines)
            .load::<MachineAddress>(connection)?
            .grouped_by(&machines);

        Ok(machines.into_iter().zip(addresses).collect::<Vec<_>>())
    })
}

fn format_wireguard_ip(wireguard_ip: &Option<IpNetwork>) -> String {
    match wireguard_ip {
        Some(ipnetwork) => ipnetwork.ip().to_string(),
        None => "-".to_string(),
    }
}

fn format_provider(provider: Option<i32>) -> String {
    match provider {
        Some(p) => p.to_string(),
        None => "-".to_string(),
    }
}

fn print_tabwriter(tw: TabWriter<Vec<u8>>) -> Result<()> {
    let bytes = tw.into_inner().context(IntoInner)?;
    std::io::stdout().write_all(&bytes).context(Io)
}

fn list_providers(connection: &PgConnection) -> Result<()> {
    let providers = providers::table
        .load::<Provider>(connection)?;

    let mut tw = TabWriter::new(vec![]);
    writeln!(tw, "ID\tNAME\tEMAIL").context(Io)?;
    writeln!(tw, "--\t----\t-----").context(Io)?;
    for provider in &providers {
        writeln!(tw, "{}\t{}\t{}",
                 provider.id,
                 provider.name,
                 provider.email
        ).context(Io)?;
    }
    print_tabwriter(tw)
}

fn list_machines(connection: &PgConnection) -> Result<()> {
    let mut data = get_machines_and_addresses(&connection)?;

    // natural_sort refuses to compare string segments with integer segments,
    // so if returns None, fall back to String cmp.
    data.sort_unstable_by(|(m1, _), (m2, _)| {
        HumanStr::new(&m1.hostname)
            .partial_cmp(&HumanStr::new(&m2.hostname))
            .unwrap_or_else(|| m1.hostname.cmp(&m2.hostname))
    });

    let mut tw = TabWriter::new(vec![]);
    writeln!(tw, "HOSTNAME\tWIREGUARD\tOWNER\tPROVIDER").context(Io)?;
    writeln!(tw, "--------\t---------\t-----\t--------").context(Io)?;
    for (machine, _addresses) in &data {
        writeln!(tw, "{}\t{}\t{}\t{}",
                 machine.hostname,
                 format_wireguard_ip(&machine.wireguard_ip),
                 machine.owner,
                 format_provider(machine.provider_id)
        ).context(Io)?;
    }
    print_tabwriter(tw)
}

fn get_existing_wireguard_ips(connection: &PgConnection) -> Result<impl Iterator<Item=IpNetwork>> {
    Ok(machines::table
        .load::<Machine>(connection)?
        .into_iter()
        .filter_map(|row| row.wireguard_ip))
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn increment_ip(ip: &Ipv4Addr) -> Option<Ipv4Addr> {
    let mut octets = ip.octets();
    if octets == [255, 255, 255, 255] {
        return None;
    }
    for i in (0..4).rev() {
        if octets[i] < 255 {
            octets[i] += 1;
            break;
        } else {
            octets[i] = 0;
        }
    }
    Some(Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]))
}

fn get_unused_wireguard_ip(connection: &PgConnection, start_ip: Ipv4Addr, end_ip: Ipv4Addr) -> Result<IpNetwork> {
    let existing = get_existing_wireguard_ips(&connection)?.collect::<HashSet<IpNetwork>>();
    let ip_iter = iter::successors(Some(start_ip), increment_ip);
    for proposed_ip in ip_iter {
        let ipnetwork = IpNetwork::new(IpAddr::V4(proposed_ip), 32).unwrap();
        if !existing.contains(&ipnetwork) {
            return Ok(ipnetwork);
        }
        if proposed_ip == end_ip {
            break;
        }
    }
    Err(Error::NoWireGuardAddressAvailable)
}

fn env_var(var: &str) -> Result<String> {
    env::var(var).context(Var { var })
}

macro_rules! unwrap_or_else {
    ($opt:expr, $else:expr) => {
        match $opt {
            Some(x) => x,
            None => $else
        }
    };
}

macro_rules! ok_or_else {
    ($opt:expr, $else:expr) => {
        match $opt {
            Some(x) => Some(x),
            None => $else
        }
    };
}

#[allow(clippy::too_many_arguments)]
fn add_machine(
    connection: &PgConnection,
    hostname: &str,
    owner: Option<String>,
    ssh_port: Option<u16>,
    ssh_user: Option<String>,
    wireguard_ip: Option<Ipv4Addr>,
    wireguard_pubkey: &Option<String>,
    provider: Option<u32>,
) -> Result<()> {
    // Required environmental variables
    let start_ip      = env_var("WIREGUARD_IP_START")?.parse::<Ipv4Addr>().context(AddrParse { var: "WIREGUARD_IP_START" })?;
    let end_ip        = env_var("WIREGUARD_IP_END")?.parse::<Ipv4Addr>().context(AddrParse { var: "WIREGUARD_IP_END" })?;
    let path_template = env_var("WIREGUARD_PRIVATE_KEY_PATH_TEMPLATE")?;
    // Optional environmntal variables
    let ssh_port      = unwrap_or_else!(ssh_port, env_var("DEFAULT_SSH_PORT")?.parse::<u16>().context(ParseInt { var: "DEFAULT_SSH_PORT" })?);
    let ssh_user      = unwrap_or_else!(ssh_user, env_var("DEFAULT_SSH_USER")?);
    let owner         = unwrap_or_else!(owner, env_var("DEFAULT_OWNER")?);
    let provider_id   = ok_or_else!(provider,
        match env_var("DEFAULT_PROVIDER") {
            Ok(s) => Some(s.parse::<u32>().context(ParseInt { var: "DEFAULT_PROVIDER" })?),
            Err(_) => None,
        }
    );

    let wireguard_ip = match wireguard_ip {
        Some(ip) => IpNetwork::new(IpAddr::V4(ip), 32).unwrap(),
        None => get_unused_wireguard_ip(&connection, start_ip, end_ip)?,
    };
    let wireguard_pubkey = match wireguard_pubkey {
        Some(pubkey) => pubkey.clone().into_bytes(),
        None => {
            let wireguard::Keypair { privkey, pubkey } = wireguard::generate_keypair()?;

            let private_key_file = rt_format!(path_template, hostname = hostname, wireguard_ip = wireguard_ip).map_err(|_| Error::FormatString)?;
            let private_key_path = Path::new(&private_key_file);
            fs::create_dir_all(private_key_path.parent().ok_or(Error::NoParentDirectory)?)?;
            let mut file = fs::File::create(private_key_file).context(Io)?;

            file.write_all(&privkey).context(Io)?;
            pubkey
        },
    };

    let machine = NewMachine {
        hostname: hostname.into(),
        wireguard_ip: Some(wireguard_ip),
        wireguard_pubkey: Some(str::from_utf8(&wireguard_pubkey).unwrap().to_string()),
        ssh_port: Some(i32::from(ssh_port)),
        ssh_user: Some(ssh_user),
        owner,
        provider_id: provider_id.map(|n| i32::try_from(n).unwrap()),
    };

    diesel::insert_into(machines::table)
        .values(&machine)
        .execute(connection)?;

    Ok(())
}

fn print_ssh_config(connection: &PgConnection, for_machine: &str) -> Result<()> {
    let (data, network_links_map) = connection.transaction::<_, Error, _>(|| {
        let data = get_machines_and_addresses(&connection)?;
        let network_links_map = get_network_links_map(&connection)?;
        Ok((data, network_links_map))
    })?;
    let source_machine = data.iter().find(|(machine, _)| machine.hostname == for_machine);
    let source_networks = match source_machine {
        None => return Err(Error::MissingSourceMachine { source_machine: for_machine.into() }),
        Some((_, addresses)) => {
            addresses.iter().map(|a| a.network.clone()).collect::<Vec<_>>()
        }
    };

    println!("# infrabase-generated SSH config for {}\n", for_machine);

    for (machine, addresses) in &data {
        let dest_networks = addresses.iter().map(|a| a.network.clone()).collect::<Vec<_>>();
        let mut network_to_network = iproduct!(&source_networks, &dest_networks)
            .filter(|(s, d)| network_links_map.contains_key(&(s.to_string(), d.to_string())))
            .collect::<Vec<_>>();
        network_to_network.sort_unstable_by_key(|(s, d)| network_links_map.get(&(s.to_string(), d.to_string())).unwrap());
        let (address, ssh_port) = match network_to_network.get(0) {
            None => {
                // We prefer to SSH over the non-WireGuard IP in case WireGuard is down,
                // but if there is no reachable address, use the WireGuard IP instead.
                (machine.wireguard_ip.map(|o| o.ip()), machine.ssh_port)
            },
            Some((_, dest_network)) => {
                let desired_address = addresses.iter().find(|a| a.network == **dest_network).unwrap();
                (Some(desired_address.address.ip()), desired_address.ssh_port)
            }
        };

        if let (Some(address), Some(port)) = (address, ssh_port) {
            println!(indoc!("
                # owner: {}
                Host {}
                  HostName {}
                  Port {}
            "), machine.owner, machine.hostname, address, port);
        }
    }
    Ok(())
}

#[derive(StructOpt, Debug)]
#[structopt(name = "infrabase")]
/// the machine inventory system
enum InfrabaseCommand {
    /// Subcommands to work with providers
    #[structopt(name = "providers")]
    Providers(ProvidersCommand),

    #[structopt(name = "ls")]
    /// List machines
    List,

    #[structopt(name = "add")]
    /// Add machine
    Add {
        /// Machine hostname
        #[structopt(name = "HOSTNAME")]
        hostname: String,

        /// Machine owner
        ///
        /// If one is not provided, DEFAULT_OWNER will be used from the environment.
        #[structopt(long)]
        owner: Option<String>,

        /// SSH port
        ///
        /// If one is not provided, DEFAULT_SSH_PORT will be used from the environment.
        #[structopt(long)]
        ssh_port: Option<u16>,

        /// SSH user
        ///
        /// If one is not provided, DEFAULT_SSH_USER will be used from the environment.
        #[structopt(long)]
        ssh_user: Option<String>,

        /// WireGuard IP
        ///
        /// If one is not provided, an unused IP address will be selected.
        #[structopt(long)]
        wireguard_ip: Option<Ipv4Addr>,

        /// WireGuard public key
        ///
        /// If one is not provided, a new private key will be generated and
        /// saved to a file specified by WIREGUARD_PRIVATE_KEY_PATH_TEMPLATE
        /// from the environment.
        #[structopt(long)]
        wireguard_pubkey: Option<String>,

        /// Provider
        ///
        /// If one is not provided, DEFAULT_OWNER will be used from the environment
        /// if set, otherwise it will be left unset.
        #[structopt(long)]
        provider: Option<u32>,
    },

    #[structopt(name = "ssh_config")]
    /// Prints an ~/.ssh/config that lists all machines
    SshConfig {
        /// Machine to generate SSH config for
        #[structopt(long = "for", name = "MACHINE")]
        r#for: String,
    },
}

#[derive(StructOpt, Debug)]
enum ProvidersCommand {
    #[structopt(name = "ls")]
    /// List providers
    List
}

fn run() -> Result<()> {
    import_env()?;
    env_logger::init();
    let connection = establish_connection()?;

    let matches = InfrabaseCommand::from_args();
    match matches {
        InfrabaseCommand::Providers(ProvidersCommand::List) => {
            list_providers(&connection)?;
        },
        InfrabaseCommand::List => {
            list_machines(&connection)?;
        },
        InfrabaseCommand::Add { hostname, owner, ssh_port, ssh_user, wireguard_ip, wireguard_pubkey, provider } => {
            add_machine(&connection, &hostname, owner, ssh_port, ssh_user, wireguard_ip, &wireguard_pubkey, provider)?;
        },
        InfrabaseCommand::SshConfig { r#for } => {
            print_ssh_config(&connection, &r#for)?;
        },
    }
    Ok(())
}

fn main() {
    std::process::exit(match run() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("An error occurred:\n{}", err);
            if let Some(bt) = err.backtrace() {
                eprintln!("{}", bt);
            }
            1
        },
    });
}

#[cfg(test)]
mod tests {
    use super::increment_ip;
    use std::net::Ipv4Addr;

    #[test]
    fn test_increment_ip() {
        assert_eq!(increment_ip(&Ipv4Addr::new(0,   0,   0,   0)),   Some(Ipv4Addr::new(0, 0, 0,   1)));
        assert_eq!(increment_ip(&Ipv4Addr::new(0,   0,   0,   1)),   Some(Ipv4Addr::new(0, 0, 0,   2)));
        assert_eq!(increment_ip(&Ipv4Addr::new(0,   0,   1,   255)), Some(Ipv4Addr::new(0, 0, 2,   0)));
        assert_eq!(increment_ip(&Ipv4Addr::new(0,   0,   255, 0)),   Some(Ipv4Addr::new(0, 0, 255, 1)));
        assert_eq!(increment_ip(&Ipv4Addr::new(0,   2,   255, 255)), Some(Ipv4Addr::new(0, 3, 0,   0)));
        assert_eq!(increment_ip(&Ipv4Addr::new(3,   255, 255, 255)), Some(Ipv4Addr::new(4, 0, 0,   0)));
        assert_eq!(increment_ip(&Ipv4Addr::new(255, 255, 255, 255)), None);
    }
}
