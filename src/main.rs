#![feature(proc_macro_hygiene)]

pub mod schema;
pub mod models;

#[macro_use]
extern crate diesel;

use std::collections::HashMap;
use std::{env, path::PathBuf};
use diesel::prelude::*;
use diesel::pg::PgConnection;
use dotenv;
use snafu::{ResultExt, Snafu};
use structopt::StructOpt;
use indoc::indoc;

use schema::{machines, network_links};
use models::{Machine, MachineAddress, NetworkLink};

#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display("Unable to read configuration from {}: {}", path.display(), source))]
    ReadConfiguration { source: dotenv::DotenvError, path: PathBuf },

    #[snafu(display("Could not find source machine {:?} in database", source_machine))]
    MissingSourceMachine { source_machine: String },
}

type Result<T, E = Error> = std::result::Result<T, E>;

fn import_env() -> Result<()> {
    let path = dirs::config_dir().unwrap().join("infrabase").join("env");
    dotenv::from_path(&path).context(ReadConfiguration { path })
}

fn establish_connection() -> PgConnection {
    let database_url = env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set");
    PgConnection::establish(&database_url)
        .expect(&format!("Error connecting to {}", database_url))
}

/// A map of (network, other_network) -> priority
type NetworkLinksMap = HashMap<(String, String), i32>;

fn networks_links_map(connection: &PgConnection) -> NetworkLinksMap {
    network_links::table
        .load::<NetworkLink>(connection)
        .expect("Error loading network_links")
        .into_iter()
        .map(|row| ((row.name, row.other_network), row.priority))
        .collect::<HashMap<_, _>>()
}

fn print_ssh_config(for_machine: &str) -> Result<()> {
    let connection = establish_connection();

    let machines = machines::table
        .load::<Machine>(&connection)
        .expect("Error loading machines");

    let addresses = MachineAddress::belonging_to(&machines)
        .load::<MachineAddress>(&connection)
        .expect("Error loading addresses")
        .grouped_by(&machines);

    let data = machines.into_iter().zip(addresses).collect::<Vec<_>>();
    let source_machine = data.iter().find(|(machine, _)| machine.hostname == for_machine);
    let source_networks = match source_machine {
        None => return Err(Error::MissingSourceMachine { source_machine: for_machine.into() }),
        Some((_, addresses)) => {
            addresses.iter().map(|a| &a.network).collect::<Vec<_>>()
        }
    };

    println!("# infrabase-generated SSH config for {}\n", for_machine);

    for (machine, addresses) in data {
        let (address, ssh_port) = match *addresses {
            [MachineAddress { address, ssh_port, .. }] => (format!("{}", address.ip()), ssh_port),
            _ => ("".into(), None),
        };
        if let Some(port) = ssh_port {
            println!(indoc!("
                # {}'s
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
enum Opt {
    #[structopt(name = "ssh_config")]
    /// Prints an ~/.ssh/config that lists all machines
    SshConfig {
        /// Machine to generate SSH config for
        #[structopt(long = "for", name = "MACHINE")]
        r#for: String,
    },
}

fn run() -> Result<()> {
    import_env()?;
    env_logger::init();

    let matches = Opt::from_args();
    match matches {
        Opt::SshConfig { r#for } => {
            print_ssh_config(&r#for)?;
        }
    }
    Ok(())
}

fn main() {
    match run() {
        Ok(())   => {},
        Err(err) => eprintln!("An error occurred:\n{}", err),
    }
}
