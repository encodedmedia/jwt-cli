use atty::Stream;
use chrono::{TimeZone, Utc};
use clap::{arg_enum, crate_authors, crate_version, App, Arg, ArgMatches, SubCommand};
use jsonwebtoken::errors::{Error, ErrorKind, Result as JWTResult};
use jsonwebtoken::{
    dangerous_insecure_decode, decode, encode, Algorithm, DecodingKey, EncodingKey, Header,
    TokenData, Validation,
};

use serde_derive::{Deserialize, Serialize};
use serde_json::{from_str, to_string_pretty, Value};
use std::collections::BTreeMap;
use std::process::exit;
use std::{fs, io, str};
use std::path::Path;
use std::ffi::OsStr;

use jsonwebkey::{JsonWebKey};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct PayloadItem(String, Value);

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Payload(BTreeMap<String, Value>);

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct TokenOutput {
    header: Header,
    payload: Payload,
}

arg_enum! {
    #[allow(clippy::clippy::upper_case_acronyms)]
    #[derive(Debug, PartialEq)]
    enum SupportedAlgorithms {
        HS256,
        HS384,
        HS512,
        RS256,
        RS384,
        RS512,
        PS256,
        PS384,
        PS512,
        ES256,
        ES384,
    }
}

arg_enum! {
    #[allow(clippy::clippy::upper_case_acronyms)]
    enum SupportedTypes {
        JWT
    }
}

enum KeyFormat {
    PEM,
    DER,
    JWK
}

#[derive(Debug, PartialEq)]
enum OutputFormat {
    Text,
    Json,
}

impl PayloadItem {
    fn from_string(val: Option<&str>) -> Option<PayloadItem> {
        val.map(|item| PayloadItem::split_payload_item(item))
    }

    fn from_string_with_name(val: Option<&str>, name: &str) -> Option<PayloadItem> {
        match val {
            Some(value) => match from_str(value) {
                Ok(json_value) => Some(PayloadItem(name.to_string(), json_value)),
                Err(_) => match from_str(format!("\"{}\"", value).as_str()) {
                    Ok(json_value) => Some(PayloadItem(name.to_string(), json_value)),
                    Err(_) => None,
                },
            },
            _ => None,
        }
    }

    // If the value is defined as systemd.time, converts the defined duration into a UNIX timestamp
    fn from_timestamp_with_name(val: Option<&str>, name: &str, now: i64) -> Option<PayloadItem> {
        if let Some(timestamp) = val {
            if timestamp.parse::<u64>().is_err() {
                let duration = parse_duration::parse(timestamp);
                if let Ok(parsed_duration) = duration {
                    let seconds = parsed_duration.as_secs() + now as u64;
                    return PayloadItem::from_string_with_name(Some(&seconds.to_string()), name);
                }
            }
        }

        PayloadItem::from_string_with_name(val, name)
    }

    fn split_payload_item(p: &str) -> PayloadItem {
        let split: Vec<&str> = p.split('=').collect();
        let (name, value) = (split[0], split[1]);
        let payload_item = PayloadItem::from_string_with_name(Some(value), name);

        payload_item.unwrap()
    }
}

impl Payload {
    fn from_payloads(payloads: Vec<PayloadItem>) -> Payload {
        let mut payload = BTreeMap::new();

        for PayloadItem(k, v) in payloads {
            payload.insert(k, v);
        }

        Payload(payload)
    }

    fn convert_timestamps(&mut self) {
        let timestamp_claims: Vec<String> = vec!["iat".into(), "nbf".into(), "exp".into()];

        for (key, value) in self.0.iter_mut() {
            if timestamp_claims.contains(key) && value.is_number() {
                *value = match value.as_i64() {
                    Some(timestamp) => Utc.timestamp(timestamp, 0).to_rfc3339().into(),
                    None => value.clone(),
                }
            }
        }
    }
}

impl SupportedAlgorithms {
    fn from_string(alg: &str) -> SupportedAlgorithms {
        match alg {
            "HS256" => SupportedAlgorithms::HS256,
            "HS384" => SupportedAlgorithms::HS384,
            "HS512" => SupportedAlgorithms::HS512,
            "RS256" => SupportedAlgorithms::RS256,
            "RS384" => SupportedAlgorithms::RS384,
            "RS512" => SupportedAlgorithms::RS512,
            "PS256" => SupportedAlgorithms::PS256,
            "PS384" => SupportedAlgorithms::PS384,
            "PS512" => SupportedAlgorithms::PS512,
            "ES256" => SupportedAlgorithms::ES256,
            "ES384" => SupportedAlgorithms::ES384,
            _ => SupportedAlgorithms::HS256,
        }
    }
}

impl TokenOutput {
    fn new(data: TokenData<Payload>) -> Self {
        TokenOutput {
            header: data.header,
            payload: data.claims,
        }
    }
}

fn config_options<'a, 'b>() -> App<'a, 'b> {
    App::new("jwt")
        .about("Encode and decode JWTs from the command line. Keys can be in PEM/DER/JWK.")
        .version(crate_version!())
        .author(crate_authors!())
        .subcommand(
            SubCommand::with_name("encode")
                .about("Encode new JWTs")
                .arg(
                    Arg::with_name("algorithm")
                        .help("the algorithm to use for signing the JWT")
                        .takes_value(true)
                        .long("alg")
                        .short("A")
                        .possible_values(&SupportedAlgorithms::variants())
                        .default_value("HS256"),
                ).arg(
                    Arg::with_name("kid")
                        .help("the kid to place in the header")
                        .takes_value(true)
                        .long("kid")
                        .short("k"),
                ).arg(
                    Arg::with_name("type")
                        .help("the type of token being encoded")
                        .takes_value(true)
                        .long("typ")
                        .short("t")
                        .possible_values(&SupportedTypes::variants()),
                ).arg(
                    Arg::with_name("json")
                        .help("the json payload to encode")
                        .index(1)
                        .required(false),
                ).arg(
                    Arg::with_name("payload")
                        .help("a key=value pair to add to the payload")
                        .number_of_values(1)
                        .multiple(true)
                        .takes_value(true)
                        .long("payload")
                        .short("P")
                        .validator(is_payload_item),
                ).arg(
                    Arg::with_name("expires")
                        .help("the time the token should expire, in seconds or systemd.time string")
                        .default_value("+30 min")
                        .takes_value(true)
                        .long("exp")
                        .short("e")
                        .validator(is_timestamp_or_duration),
                ).arg(
                    Arg::with_name("issuer")
                        .help("the issuer of the token")
                        .takes_value(true)
                        .long("iss")
                        .short("i"),
                ).arg(
                    Arg::with_name("subject")
                        .help("the subject of the token")
                        .takes_value(true)
                        .long("sub")
                        .short("s"),
                ).arg(
                    Arg::with_name("audience")
                        .help("the audience of the token")
                        .takes_value(true)
                        .long("aud")
                        .short("a")
                ).arg(
                    Arg::with_name("jwt_id")
                        .help("the jwt id of the token")
                        .takes_value(true)
                        .long("jti")
                ).arg(
                    Arg::with_name("not_before")
                        .help("the time the JWT should become valid, in seconds or systemd.time string")
                        .takes_value(true)
                        .long("nbf")
                        .short("n")
                        .validator(is_timestamp_or_duration),
                ).arg(
                    Arg::with_name("no_iat")
                        .help("prevent an iat claim from being automatically added")
                        .long("no-iat")
                ).arg(
                    Arg::with_name("secret")
                        .help("the secret to sign the JWT with. Can be prefixed with @ to read from a file")
                        .takes_value(true)
                        .long("secret")
                        .short("S")
                        .required(true),
                ).arg(
                    Arg::with_name("keyformat")
                        .help("the format of the secret param or file: pem|der|jwk are supported. Default: pem")
                        .takes_value(true)
                        .long("keyformat")
                        .short("f")
                        .required(false),
                ),
        ).subcommand(
            SubCommand::with_name("decode")
                .about("Decode a JWT")
                .arg(
                    Arg::with_name("jwt")
                        .help("the jwt to decode")
                        .index(1)
                        .required(true),
                ).arg(
                    Arg::with_name("algorithm")
                        .help("the algorithm to use for signing the JWT")
                        .takes_value(true)
                        .long("alg")
                        .short("A")
                        .possible_values(&SupportedAlgorithms::variants())
                        .default_value("HS256"),
                ).arg(
                    Arg::with_name("iso_dates")
                        .help("display unix timestamps as ISO 8601 dates")
                        .takes_value(false)
                        .long("iso8601")
                ).arg(
                    Arg::with_name("secret")
                        .help("the secret to validate the JWT with. Can be prefixed with @ to read from a file")
                        .takes_value(true)
                        .long("secret")
                        .short("S")
                        .default_value(""),
                ).arg(
                    Arg::with_name("json")
                        .help("render decoded JWT as JSON")
                        .long("json")
                        .short("j"),
                ).arg(
                    Arg::with_name("ignore_exp")
                        .help("Ignore token expiration date (`exp` claim) during validation.")
                        .long("ignore-exp")
                ).arg(
                    Arg::with_name("keyformat")
                        .help("the format of the secret param or file: pem|der|jwk are supported. Default: pem")
                        .takes_value(true)
                        .long("keyformat")
                        .short("f")
                        .required(false),
                ),
        )
}

fn is_timestamp_or_duration(val: String) -> Result<(), String> {
    match val.parse::<i64>() {
        Ok(_) => Ok(()),
        Err(_) => match parse_duration::parse(&val) {
            Ok(_) => Ok(()),
            Err(_) => Err(String::from(
                "must be a UNIX timestamp or systemd.time string",
            )),
        },
    }
}

fn is_payload_item(val: String) -> Result<(), String> {
    match val.split('=').count() {
        2 => Ok(()),
        _ => Err(String::from(
            "payloads must have a key and value in the form key=value",
        )),
    }
}

fn warn_unsupported(matches: &ArgMatches) {
    if matches.value_of("type").is_some() {
        println!("Sorry, `typ` isn't supported quite yet!");
    }
}

fn translate_algorithm(alg: SupportedAlgorithms) -> Algorithm {
    match alg {
        SupportedAlgorithms::HS256 => Algorithm::HS256,
        SupportedAlgorithms::HS384 => Algorithm::HS384,
        SupportedAlgorithms::HS512 => Algorithm::HS512,
        SupportedAlgorithms::RS256 => Algorithm::RS256,
        SupportedAlgorithms::RS384 => Algorithm::RS384,
        SupportedAlgorithms::RS512 => Algorithm::RS512,
        SupportedAlgorithms::PS256 => Algorithm::PS256,
        SupportedAlgorithms::PS384 => Algorithm::PS384,
        SupportedAlgorithms::PS512 => Algorithm::PS512,
        SupportedAlgorithms::ES256 => Algorithm::ES256,
        SupportedAlgorithms::ES384 => Algorithm::ES384,
    }
}

fn create_header(alg: Algorithm, kid: Option<&str>) -> Header {
    let mut header = Header::new(alg);

    header.kid = kid.map(str::to_string);

    header
}

fn slurp_file(file_name: &str) -> Vec<u8> {
    fs::read(file_name).unwrap_or_else(|_| panic!("Unable to read file {}", file_name))
}

fn encoding_key_from_secret(alg: &Algorithm, secret_string: &str, formatopt: Option<&str>) -> JWTResult<EncodingKey> {
    let secret = 
        if secret_string.starts_with('@') {
            slurp_file(&secret_string.chars().skip(1).collect::<String>())
        } else {
            secret_string.as_bytes().to_vec()
        };        
    
    let format = 
        match formatopt {
            None => {
                if secret_string.starts_with('@'){
                    match Path::new(secret_string).extension().and_then(OsStr::to_str) {
                        Some("pem") | Some("cer") | Some("key") => KeyFormat::PEM,
                        Some("der") => KeyFormat::DER,
                        Some("jwk") => KeyFormat::JWK,
                        _ => KeyFormat::PEM
                    }
                } else {
                    KeyFormat::PEM
                }
            }
            Some("pem") => KeyFormat::PEM,
            Some("der") => KeyFormat::DER,
            Some("jwk") => KeyFormat::JWK,
            Some(_) => KeyFormat::PEM
        };

    match alg {
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
            Ok(EncodingKey::from_secret(&secret))            
        }
        Algorithm::RS256
        | Algorithm::RS384
        | Algorithm::RS512
        | Algorithm::PS256
        | Algorithm::PS384
        | Algorithm::PS512 => {            
            match format {
                KeyFormat::PEM => EncodingKey::from_rsa_pem(&secret),
                KeyFormat::DER => Ok(EncodingKey::from_rsa_der(&secret)),
                KeyFormat::JWK => {
                    let jwk: JsonWebKey = str::from_utf8(&secret).unwrap().parse().unwrap();
                    EncodingKey::from_rsa_pem(&jwk.key.to_pem().as_bytes())
                }
            }
        }
        Algorithm::ES256 | Algorithm::ES384 => {        
            match format {
                KeyFormat::PEM => EncodingKey::from_ec_pem(&secret),
                KeyFormat::DER => Ok(EncodingKey::from_ec_der(&secret)),
                KeyFormat::JWK => {
                    let jwk: JsonWebKey = str::from_utf8(&secret).unwrap().parse().unwrap();
                    EncodingKey::from_ec_pem(&jwk.key.to_pem().as_bytes())
                }
            }
        }
    }
}

fn decoding_key_from_secret(
    alg: &Algorithm,
    secret_string: &str,
    formatopt: Option<&str>,
    kid: Option<&String>
) -> JWTResult<DecodingKey<'static>> {
    let secret = 
        if secret_string.starts_with('@') {
            slurp_file(&secret_string.chars().skip(1).collect::<String>())
        } else {
            secret_string.as_bytes().to_vec()
        };        
    
    let format = 
        match formatopt {
            None => {
                if secret_string.starts_with('@'){
                    match Path::new(secret_string).extension().and_then(OsStr::to_str) {
                        Some("pem") | Some("cer") | Some("key") => KeyFormat::PEM,
                        Some("der") => KeyFormat::DER,
                        Some("jwk") => KeyFormat::JWK,
                        _ => KeyFormat::PEM
                    }
                } else {
                    KeyFormat::PEM
                }
            }
            Some("pem") => KeyFormat::PEM,
            Some("der") => KeyFormat::DER,
            Some("jwk") => KeyFormat::JWK,
            Some(_) => KeyFormat::PEM
        };
    
    let selected_key = match (&format, kid) {
        (KeyFormat::JWK, Some(kid)) => {
            let obj: Value = serde_json::from_str(str::from_utf8(&secret).unwrap())?;            
            match &obj["keys"] {                
                Value::Array(ar) => {
                    match ar.iter().find(|x| match &x["kid"] {
                        Value::String(s) => kid.eq(s),
                        _ => false
                    }) {
                        Some(kobj) => {                            
                            Some(serde_json::to_string(&kobj)?)
                        },
                        _ => return Err(Error::from(ErrorKind::InvalidSignature))
                    }
                }                        
                _ => Some(String::from_utf8(secret.clone())?),
            }
        },
        _ => None
    };

    
    match alg {
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
            Ok(DecodingKey::from_secret(&secret).into_static())            
        }
        Algorithm::RS256
        | Algorithm::RS384
        | Algorithm::RS512
        | Algorithm::PS256
        | Algorithm::PS384
        | Algorithm::PS512 => {
            match format {
                KeyFormat::PEM => DecodingKey::from_rsa_pem(&secret).map(DecodingKey::into_static),
                KeyFormat::DER => Ok(DecodingKey::from_rsa_der(&secret).into_static()),
                KeyFormat::JWK => {
                    let jwk: JsonWebKey = selected_key.unwrap().parse().unwrap();
                    DecodingKey::from_rsa_pem(&jwk.key.to_pem().as_bytes()).map(DecodingKey::into_static)
                }
            }            
        }
        Algorithm::ES256 | Algorithm::ES384 => {
            match format {
                KeyFormat::PEM => DecodingKey::from_ec_pem(&secret).map(DecodingKey::into_static),
                KeyFormat::DER => Ok(DecodingKey::from_ec_der(&secret).into_static()),
                KeyFormat::JWK => {
                    let jwk: JsonWebKey = selected_key.unwrap().parse().unwrap();
                    DecodingKey::from_ec_pem(&jwk.key.to_pem().as_bytes()).map(DecodingKey::into_static)                    
                }
            }
        }
    }
}

fn encode_token(matches: &ArgMatches) -> JWTResult<String> {
    let algorithm = translate_algorithm(SupportedAlgorithms::from_string(
        matches.value_of("algorithm").unwrap(),
    ));
    let kid = matches.value_of("kid");
    let header = create_header(algorithm, kid);
    let custom_payloads: Option<Vec<Option<PayloadItem>>> =
        matches.values_of("payload").map(|maybe_payloads| {
            maybe_payloads
                .map(|p| PayloadItem::from_string(Some(p)))
                .collect()
        });
    let custom_payload = matches
        .value_of("json")
        .map(|value| {
            if value != "-" {
                return String::from(value);
            }

            let mut buffer = String::new();

            io::stdin()
                .read_line(&mut buffer)
                .expect("STDIN was not valid UTF-8");

            buffer
        })
        .map(|raw_json| match from_str(&raw_json) {
            Ok(Value::Object(json_value)) => json_value
                .into_iter()
                .map(|(json_key, json_val)| Some(PayloadItem(json_key, json_val)))
                .collect(),
            _ => panic!("Invalid JSON provided!"),
        });
    let now = Utc::now().timestamp();
    let expires = match matches.occurrences_of("expires") {
        0 => None,
        _ => PayloadItem::from_timestamp_with_name(matches.value_of("expires"), "exp", now),
    };
    let not_before =
        PayloadItem::from_timestamp_with_name(matches.value_of("not_before"), "nbf", now);
    let issued_at = match matches.is_present("no_iat") {
        true => None,
        false => PayloadItem::from_timestamp_with_name(Some(&now.to_string()), "iat", now),
    };
    let issuer = PayloadItem::from_string_with_name(matches.value_of("issuer"), "iss");
    let subject = PayloadItem::from_string_with_name(matches.value_of("subject"), "sub");
    let audience = PayloadItem::from_string_with_name(matches.value_of("audience"), "aud");
    let jwt_id = PayloadItem::from_string_with_name(matches.value_of("jwt_id"), "jti");
    let mut maybe_payloads: Vec<Option<PayloadItem>> = vec![
        issued_at, expires, issuer, subject, audience, jwt_id, not_before,
    ];

    maybe_payloads.append(&mut custom_payloads.unwrap_or_default());
    maybe_payloads.append(&mut custom_payload.unwrap_or_default());

    let payloads = maybe_payloads.into_iter().flatten().collect();
    let Payload(claims) = Payload::from_payloads(payloads);

    encoding_key_from_secret(&algorithm, matches.value_of("secret").unwrap(), matches.value_of("keyformat"))
        .and_then(|secret| encode(&header, &claims, &secret))
}

fn decode_token(
    matches: &ArgMatches,
) -> (
    JWTResult<TokenData<Payload>>,
    JWTResult<TokenData<Payload>>,
    OutputFormat,
) {
    let algorithm = translate_algorithm(SupportedAlgorithms::from_string(
        matches.value_of("algorithm").unwrap(),
    ));
    
    let jwt = matches
        .value_of("jwt")
        .map(|value| {
            if value != "-" {
                return String::from(value);
            }

            let mut buffer = String::new();

            io::stdin()
                .read_line(&mut buffer)
                .expect("STDIN was not valid UTF-8");

            buffer
        })
        .unwrap()
        .trim()
        .to_owned();

    let secret_validator = Validation {
        leeway: 1000,
        algorithms: vec![algorithm],
        validate_exp: !matches.is_present("ignore_exp"),
        ..Default::default()
    };

    let token_data = dangerous_insecure_decode::<Payload>(&jwt).map(|mut token| {
        if matches.is_present("iso_dates") {
            token.claims.convert_timestamps();
        }

        token
    });

    let kid = match &token_data{
        Ok(token) => match &token.header.kid{
            Some(kid) => Some(kid),
            _ => None
        },
        _ => None
    };

    let ofmt = if matches.is_present("json") {
        OutputFormat::Json
    } else {
        OutputFormat::Text
    };

    let secret = match matches.value_of("secret").map(|s| (s, !s.is_empty())) {
        Some((secret, true)) => match decoding_key_from_secret(&algorithm, secret, matches.value_of("keyformat"), kid) {
            Ok(val) => Some(val),
            Err(kind) => return (Err(kind), token_data, ofmt)
        },
        _ => None,
    };

    (
        match secret {
            Some(secret_key) => decode::<Payload>(&jwt, &secret_key, &secret_validator),
            None => dangerous_insecure_decode::<Payload>(&jwt),
        },
        token_data,
        ofmt,
    )
}

fn print_encoded_token(token: JWTResult<String>) {
    match token {
        Ok(jwt) => {
            if atty::is(Stream::Stdout) {
                println!("{}", jwt);
            } else {
                print!("{}", jwt);
            }
            exit(0);
        }
        Err(err) => {
            bunt::eprintln!("{$red+bold}Something went awry creating the jwt{/$}\n");
            eprintln!("{}", err);
            exit(1);
        }
    }
}

fn print_decoded_token(
    validated_token: JWTResult<TokenData<Payload>>,
    token_data: JWTResult<TokenData<Payload>>,
    format: OutputFormat,
) {
    if let Err(err) = &validated_token {
        match err.kind() {
            ErrorKind::InvalidToken => {
                bunt::println!("{$red+bold}The JWT provided is invalid{/$}")
            }
            ErrorKind::InvalidSignature => {
                bunt::eprintln!("{$red+bold}The JWT provided has an invalid signature{/$}")
            }
            ErrorKind::InvalidRsaKey => {
                bunt::eprintln!("{$red+bold}The secret provided isn't a valid RSA key{/$}")
            }
            ErrorKind::InvalidEcdsaKey => {
                bunt::eprintln!("{$red+bold}The secret provided isn't a valid ECDSA key{/$}")
            }
            ErrorKind::ExpiredSignature => {
                bunt::eprintln!("{$red+bold}The token has expired (or the `exp` claim is not set). This error can be ignored via the `--ignore-exp` parameter.{/$}")
            }
            ErrorKind::InvalidIssuer => {
                bunt::println!("{$red+bold}The token issuer is invalid{/$}")
            }
            ErrorKind::InvalidAudience => {
                bunt::eprintln!("{$red+bold}The token audience doesn't match the subject{/$}")
            }
            ErrorKind::InvalidSubject => {
                bunt::eprintln!("{$red+bold}The token subject doesn't match the audience{/$}")
            }
            ErrorKind::ImmatureSignature => bunt::eprintln!(
                "{$red+bold}The `nbf` claim is in the future which isn't allowed{/$}"
            ),
            ErrorKind::InvalidAlgorithm => bunt::eprintln!(
                "{$red+bold}The JWT provided has a different signing algorithm than the one you \
                     provided{/$}",
            ),
            _ => bunt::eprintln!(
                "{$red+bold}The JWT provided is invalid because{/$} {:?}",
                err
            ),
        };
    }

    match (format, token_data) {
        (OutputFormat::Json, Ok(token)) => {
            println!("{}", to_string_pretty(&TokenOutput::new(token)).unwrap())
        }
        (_, Ok(token)) => {
            bunt::println!("\n{$bold}Token header\n------------{/$}");
            println!("{}\n", to_string_pretty(&token.header).unwrap());
            bunt::println!("{$bold}Token claims\n------------{/$}");
            println!("{}", to_string_pretty(&token.claims).unwrap());
        }
        (_, Err(_)) => exit(1),
    }

    exit(match validated_token {
        Err(_) => 1,
        Ok(_) => 0,
    })
}

fn main() {
    let matches = config_options().get_matches();

    match matches.subcommand() {
        ("encode", Some(encode_matches)) => {
            warn_unsupported(encode_matches);

            let token = encode_token(encode_matches);

            print_encoded_token(token);
        }
        ("decode", Some(decode_matches)) => {
            let (validated_token, token_data, format) = decode_token(decode_matches);

            print_decoded_token(validated_token, token_data, format);
        }
        _ => (),
    }
}
