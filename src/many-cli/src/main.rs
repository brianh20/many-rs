use anyhow::anyhow;
use clap::{ArgGroup, Parser};
use coset::{CborSerializable, CoseSign1};
use many::hsm::{Hsm, HsmMechanismType, HsmSessionType, HsmUserType};
use many::message::{
    decode_response_from_cose_sign1, encode_cose_sign1_from_request, RequestMessage,
    RequestMessageBuilder, ResponseMessage,
};
use many::server::module::ledger;
use many::server::module::r#async::attributes::AsyncAttribute;
use many::server::module::r#async::{StatusArgs, StatusReturn};
use many::transport::http::HttpServer;
use many::types::identity::CoseKeyIdentity;
use many::{Identity, ManyServer};
use many_client::ManyClient;
use std::convert::TryFrom;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process;
use std::time::Duration;
use tracing::{error, info, level_filters::LevelFilter, trace};
use url::Url;

#[derive(Parser)]
struct Opts {
    /// Increase output logging verbosity to DEBUG level.
    #[clap(short, long, parse(from_occurrences))]
    verbose: i8,

    /// Suppress all output logging. Can be used multiple times to suppress more.
    #[clap(short, long, parse(from_occurrences))]
    quiet: i8,

    #[clap(subcommand)]
    subcommand: SubCommand,
}

#[derive(Parser)]
enum SubCommand {
    /// Transform a textual ID into its hexadecimal value, or the other way around.
    /// If the argument is neither hexadecimal value or identity, try to see if it's
    /// a file, and will parse it as a PEM file.
    Id(IdOpt),

    /// Display the textual ID of a public key located on an HSM.
    HsmId(HsmIdOpt),

    /// Creates a message and output it.
    Message(MessageOpt),

    /// Starts a base server that can also be used for reverse proxying
    /// to another MANY server.
    Server(ServerOpt),

    /// Get the token ID per string of a ledger's token.
    GetTokenId(GetTokenIdOpt),
}

#[derive(Parser)]
struct IdOpt {
    /// An hexadecimal value to encode, an identity textual format to decode or
    /// a PEM file to read
    arg: String,

    /// Allow to generate the identity with a specific subresource ID.
    subid: Option<u32>,
}

#[derive(Parser)]
struct HsmIdOpt {
    /// HSM PKCS#11 module path
    module: PathBuf,

    /// HSM PKCS#11 slot ID
    slot: u64,

    /// HSM PKCS#11 key ID
    keyid: String,

    /// Allow to generate the identity with a specific subresource ID.
    subid: Option<u32>,
}

#[derive(Parser)]
#[clap(
    group(
        ArgGroup::new("hsm")
            .multiple(true)
            .args(&["module", "slot", "keyid"])
            .requires_all(&["module", "slot", "keyid"])
    ),
    group(
        ArgGroup::new("action")
            .args(&["server", "hex", "base64"])
            .required(true)
    )
)]
struct MessageOpt {
    /// A pem file to sign the message. If this is omitted, the message will be anonymous.
    #[clap(long)]
    pem: Option<PathBuf>,

    /// Timestamp.
    #[clap(long)]
    timestamp: Option<String>,

    /// The server to connect to.
    #[clap(long)]
    server: Option<url::Url>,

    /// If true, prints out the hex value of the message bytes.
    #[clap(long)]
    hex: bool,

    /// If true, prints out the base64 value of the message bytes.
    #[clap(long)]
    base64: bool,

    /// If used, send the message from hexadecimal to the server and wait for
    /// the response.
    #[clap(long, requires("server"))]
    from_hex: Option<String>,

    /// Show the async token and exit right away. By default, will poll for the
    /// result of the async operation.
    #[clap(long)]
    r#async: bool,

    /// The identity to send it to.
    #[clap(long)]
    to: Option<Identity>,

    /// HSM PKCS#11 module path
    #[clap(long, conflicts_with("pem"))]
    module: Option<PathBuf>,

    /// HSM PKCS#11 slot ID
    #[clap(long, conflicts_with("pem"))]
    slot: Option<u64>,

    /// HSM PKCS#11 key ID
    #[clap(long, conflicts_with("pem"))]
    keyid: Option<String>,

    /// The method to call.
    method: Option<String>,

    /// The content of the message itself (its payload).
    data: Option<String>,
}

#[derive(Parser)]
struct ServerOpt {
    /// The location of a PEM file for the identity of this server.
    #[clap(long)]
    pem: PathBuf,

    /// The address and port to bind to for the MANY Http server.
    #[clap(long, short, default_value = "127.0.0.1:8000")]
    addr: SocketAddr,

    /// The name to give the server.
    #[clap(long, short, default_value = "many-server")]
    name: String,
}

#[derive(Parser)]
struct GetTokenIdOpt {
    /// The server to call. It MUST implement the ledger attribute (2).
    server: url::Url,

    /// The token to get. If not listed in the list of tokens, this will
    /// error.
    symbol: String,
}

fn show_response(
    response: ResponseMessage,
    client: ManyClient,
    r#async: bool,
) -> Result<(), anyhow::Error> {
    let ResponseMessage {
        data, attributes, ..
    } = response;

    let payload = data?;
    if payload.is_empty() {
        let attr = attributes.get::<AsyncAttribute>().unwrap();
        info!("Async token: {}", hex::encode(&attr.token));

        // Allow eprint/ln for showing the progress bar, when we're interactive.
        #[allow(clippy::print_stderr)]
        fn progress(str: &str, done: bool) {
            if atty::is(atty::Stream::Stderr) {
                if done {
                    eprintln!("{}", str);
                } else {
                    eprint!("{}", str);
                }
            }
        }

        if !r#async {
            progress("Waiting.", false);

            // TODO: improve on this by using duration and thread and watchdog.
            // Wait for the server for ~60 seconds by pinging it every second.
            for _ in 0..60 {
                let response = client.call(
                    "async.status",
                    StatusArgs {
                        token: attr.token.clone(),
                    },
                )?;
                let status: StatusReturn = minicbor::decode(&response.data?)?;
                match status {
                    StatusReturn::Done { response } => {
                        progress(".", true);
                        return show_response(*response, client, r#async);
                    }
                    StatusReturn::Expired => {
                        progress(".", true);
                        info!("Async token expired before we could check it.");
                        return Ok(());
                    }
                    _ => {
                        progress(".", false);
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        }
    } else {
        println!(
            "{}",
            cbor_diag::parse_bytes(&payload).unwrap().to_diag_pretty()
        );
    }

    Ok(())
}

fn message(
    s: Url,
    to: Identity,
    key: CoseKeyIdentity,
    method: String,
    data: Vec<u8>,
    r#async: bool,
) -> Result<(), anyhow::Error> {
    let client = ManyClient::new(s, to, key).unwrap();
    let response = client.call_raw(method, &data)?;

    show_response(response, client, r#async)
}

fn message_from_hex(
    s: Url,
    to: Identity,
    key: CoseKeyIdentity,
    hex: String,
    r#async: bool,
) -> Result<(), anyhow::Error> {
    let client = ManyClient::new(s.clone(), to, key).unwrap();

    let data = hex::decode(hex)?;
    let envelope = CoseSign1::from_slice(&data).map_err(|e| anyhow!(e))?;

    let cose_sign1 = ManyClient::send_envelope(s, envelope)?;
    let response = decode_response_from_cose_sign1(cose_sign1, None).map_err(|e| anyhow!(e))?;

    show_response(response, client, r#async)
}

fn main() {
    let Opts {
        verbose,
        quiet,
        subcommand,
    } = Opts::parse();
    let verbose_level = 2 + verbose - quiet;
    let log_level = match verbose_level {
        x if x > 3 => LevelFilter::TRACE,
        3 => LevelFilter::DEBUG,
        2 => LevelFilter::INFO,
        1 => LevelFilter::WARN,
        0 => LevelFilter::ERROR,
        x if x < 0 => LevelFilter::OFF,
        _ => unreachable!(),
    };
    tracing_subscriber::fmt().with_max_level(log_level).init();

    match subcommand {
        SubCommand::Id(o) => {
            if let Ok(data) = hex::decode(&o.arg) {
                match Identity::try_from(data.as_slice()) {
                    Ok(mut i) => {
                        if let Some(subid) = o.subid {
                            i = i
                                .with_subresource_id(subid)
                                .expect("Invalid subresource id");
                        }
                        println!("{}", i)
                    }
                    Err(e) => {
                        error!("Identity did not parse: {:?}", e.to_string());
                        std::process::exit(1);
                    }
                }
            } else if let Ok(mut i) = Identity::try_from(o.arg.clone()) {
                if let Some(subid) = o.subid {
                    i = i
                        .with_subresource_id(subid)
                        .expect("Invalid subresource id");
                }
                println!("{}", hex::encode(&i.to_vec()));
            } else if let Ok(pem_content) = std::fs::read_to_string(&o.arg) {
                // Create the identity from the public key hash.
                let mut i = CoseKeyIdentity::from_pem(&pem_content).unwrap().identity;
                if let Some(subid) = o.subid {
                    i = i
                        .with_subresource_id(subid)
                        .expect("Invalid subresource id");
                }

                println!("{}", i);
            } else {
                error!("Could not understand the argument.");
                std::process::exit(2);
            }
        }
        SubCommand::HsmId(o) => {
            let keyid = hex::decode(o.keyid).expect("Failed to decode keyid to hex");

            {
                let mut hsm = Hsm::get_instance().expect("HSM mutex poisoned");
                hsm.init(o.module, keyid)
                    .expect("Failed to initialize HSM module");

                // The session will stay open until the application terminates
                hsm.open_session(o.slot, HsmSessionType::RO, None, None)
                    .expect("Failed to open HSM session");
            }

            let mut id = CoseKeyIdentity::from_hsm(HsmMechanismType::ECDSA)
                .expect("Unable to create CoseKeyIdentity from HSM")
                .identity;

            if let Some(subid) = o.subid {
                id = id
                    .with_subresource_id(subid)
                    .expect("Invalid subresource id");
            }

            println!("{}", id);
        }
        SubCommand::Message(o) => {
            let key = if let (Some(module), Some(slot), Some(keyid)) = (o.module, o.slot, o.keyid) {
                trace!("Getting user PIN");
                let pin = rpassword::prompt_password("Please enter the HSM user PIN: ")
                    .expect("I/O error when reading HSM PIN");
                let keyid = hex::decode(keyid).expect("Failed to decode keyid to hex");

                {
                    let mut hsm = Hsm::get_instance().expect("HSM mutex poisoned");
                    hsm.init(module, keyid)
                        .expect("Failed to initialize HSM module");

                    // The session will stay open until the application terminates
                    hsm.open_session(slot, HsmSessionType::RO, Some(HsmUserType::User), Some(pin))
                        .expect("Failed to open HSM session");
                }

                trace!("Creating CoseKeyIdentity");
                // Only ECDSA is supported at the moment. It should be easy to add support for new EC mechanisms
                CoseKeyIdentity::from_hsm(HsmMechanismType::ECDSA)
                    .expect("Unable to create CoseKeyIdentity from HSM")
            } else if o.pem.is_some() {
                // If `pem` is not provided, use anonymous and don't sign.
                o.pem.map_or_else(CoseKeyIdentity::anonymous, |p| {
                    CoseKeyIdentity::from_pem(&std::fs::read_to_string(&p).unwrap()).unwrap()
                })
            } else {
                CoseKeyIdentity::anonymous()
            };

            let from_identity = key.identity;
            let to_identity = o.to.unwrap_or_default();

            let data = o
                .data
                .map_or(vec![], |d| cbor_diag::parse_diag(&d).unwrap().to_bytes());

            if let Some(s) = o.server {
                let result = if let Some(hex) = o.from_hex {
                    message_from_hex(s, to_identity, key, hex, o.r#async)
                } else {
                    message(
                        s,
                        to_identity,
                        key,
                        o.method.expect("--method is required"),
                        data,
                        o.r#async,
                    )
                };

                match result {
                    Ok(()) => {}
                    Err(err) => {
                        error!(
                            "Error returned by server:\n|  {}\n",
                            err.to_string()
                                .split('\n')
                                .collect::<Vec<&str>>()
                                .join("\n|  ")
                        );
                        std::process::exit(1);
                    }
                }
            } else {
                let message: RequestMessage = RequestMessageBuilder::default()
                    .version(1)
                    .from(from_identity)
                    .to(to_identity)
                    .method(o.method.expect("--method is required"))
                    .data(data)
                    .build()
                    .unwrap();

                let cose = encode_cose_sign1_from_request(message, &key).unwrap();
                let bytes = cose.to_vec().unwrap();
                if o.hex {
                    println!("{}", hex::encode(&bytes));
                } else if o.base64 {
                    println!("{}", base64::encode(&bytes));
                } else {
                    panic!("Must specify one of hex, base64 or server...");
                }
            }
        }
        SubCommand::Server(o) => {
            let pem = std::fs::read_to_string(&o.pem).expect("Could not read PEM file.");
            let key = CoseKeyIdentity::from_pem(&pem)
                .expect("Could not generate identity from PEM file.");

            let many = ManyServer::simple(
                o.name,
                key,
                Some(std::env!("CARGO_PKG_VERSION").to_string()),
                None,
            );
            HttpServer::new(many).bind(o.addr).unwrap();
        }
        SubCommand::GetTokenId(o) => {
            let client = ManyClient::new(
                o.server,
                Identity::anonymous(),
                CoseKeyIdentity::anonymous(),
            )
            .expect("Could not create a client");
            let status = client.status().expect("Cannot get status of server");

            if !status.attributes.contains(&ledger::LEDGER_MODULE_ATTRIBUTE) {
                error!("Server does not implement Ledger Attribute.");
                process::exit(1);
            }

            let info: ledger::InfoReturns = minicbor::decode(
                &client
                    .call("ledger.info", ledger::InfoArgs {})
                    .unwrap()
                    .data
                    .expect("An error happened during the call to ledger.info"),
            )
            .expect("Invalid data returned by server; not CBOR");

            let symbol = o.symbol;
            let id = info
                .local_names
                .into_iter()
                .find(|(_, y)| y == &symbol)
                .map(|(x, _)| x)
                .ok_or_else(|| format!("Could not resolve symbol '{}'", &symbol))
                .unwrap();

            println!("{}", id);
        }
    }
}
