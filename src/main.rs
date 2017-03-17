#![feature(box_syntax)]

extern crate bins as lib;
extern crate hyper;
extern crate hyper_openssl;
extern crate url;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
#[macro_use]
extern crate clap;
extern crate toml;
extern crate flate2;
#[macro_use]
extern crate log;
extern crate time;
#[cfg(feature = "file_type_checking")]
extern crate magic;

macro_rules! option {
  ($e: expr) => {{
    match $e {
      Some(x) => x,
      None => return None
    }
  }}
}

mod bins;
mod config;
mod logger;

use config::*;

use lib::*;
use lib::error::*;
use lib::files::{Paste, UploadFile};

use clap::{App, Arg, ArgMatches};
use flate2::read::GzDecoder;

use std::path::{Path, PathBuf};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::io::{Read, Write};
use std::io::Result as IoResult;
use std::error::Error;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use log::LogLevel;

use url::Url;

macro_rules! report_error_using {
  ($using: ident, $fmt: expr, $e: expr $(, $args: expr),*) => {{
    $using!($fmt, $e, $($args)*);
    for error in error_parents(&$e) {
      $using!("{}", error);
    }
  }}
}

macro_rules! report_error {
  ($fmt: expr, $e: expr $(, $args: expr),*) => (report_error_using!(error, $fmt, $e $(, $args)*))
}

fn main() {
  std::process::exit(inner());
}

fn inner() -> i32 {
  let config = match get_config() {
    Ok(c) => c,
    Err(e) => {
      report_error_using!(println, "could not create or load bins config file: {}", e);
      return 1;
    }
  };

  let matches = App::new("bins")
    .about("A tool for pasting from the terminal")
    .author(crate_authors!())
    .version(crate_version!())
    .version_message("print version information and exit")
    .version_short("v")
    .help_message("print help information and exit")
    .arg(Arg::with_name("inputs")
      .help("inputs to the program, either files or URLs")
      .takes_value(true)
      .value_name("input")
      .multiple(true))
    .arg(Arg::with_name("debug")
      .long("debug")
      .short("d")
      .help("enable debug output"))
    .arg(Arg::with_name("bin")
      .long("bin")
      .short("b")
      .help("specify the upload bin")
      .required(config.defaults.bin.is_none())
      .takes_value(true)
      .value_name("bin")
      .possible_values(&["hastebin", "sprunge", "gist"]))
    .arg(Arg::with_name("public")
      .long("public")
      .short("P")
      .help("set the paste to be public")
      .conflicts_with("private"))
    .arg(Arg::with_name("private")
      .long("private")
      .short("p")
      .help("set the paste to be private"))
    .arg(Arg::with_name("authed")
      .long("authed")
      .short("a")
      .help("set the paste to be uploaded while authenticated")
      .conflicts_with("anonymous"))
    .arg(Arg::with_name("anonymous")
      .long("anonymous")
      .short("A")
      .help("set the paste to be uploaded while not authenticated"))
    .arg(Arg::with_name("json")
      .long("json")
      .short("j")
      .help("output JSON information"))
    .arg(Arg::with_name("raw-urls")
      .long("raw-urls")
      .short("r")
      .help("output URLs to the raw content")
      .conflicts_with("html-urls"))
    .arg(Arg::with_name("html-urls")
      .long("html-urls")
      .short("u")
      .help("output URLs to the HTML content"))
    .arg(Arg::with_name("message")
      .long("message")
      .short("m")
      .help("specify a message to upload instead of files or stdin")
      .takes_value(true)
      .value_name("message")
      .conflicts_with("inputs"))
    .arg(Arg::with_name("list-bins")
      .long("list-bins")
      .short("-l")
      .help("list the available bins")
      .conflicts_with_all(&["bin",
        "public",
        "private",
        "anonymous",
        "authed",
        "raw-urls",
        "html-urls",
        "message"]))
      .arg(Arg::with_name("force")
        .long("force")
        .short("f")
        .help("force upload, ignoring safety features"))
    .get_matches();

  let level = if matches.is_present("debug") {
    LogLevel::Debug
  } else {
    LogLevel::Info
  };
  if let Err(e) = logger::Logger::new(level).init() {
    report_error_using!(println, "could not initialize logger: {}", e);
    return 1;
  }

  let mut cli_options = CommandLineOptions::default();

  if matches.is_present("public") {
    cli_options.private = Some(false);
  } else if matches.is_present("private") {
    cli_options.private = Some(true);
  }

  if matches.is_present("authed") {
    cli_options.authed = Some(true);
  } else if matches.is_present("anonymous") {
    cli_options.authed = Some(false);
  }

  if matches.is_present("json") {
    cli_options.json = Some(true);
  }

  if matches.is_present("force") {
    cli_options.force = Some(true);
  }

  if matches.is_present("raw-urls") {
    cli_options.url_output = Some(UrlOutputMode::Raw);
  } else if matches.is_present("html-urls") {
    cli_options.url_output = Some(UrlOutputMode::Html);
  }

  let config = Arc::new(config);
  let cli_options = Arc::new(cli_options);

  let bins: BTreeMap<String, Box<Bin>> = {
    let bins: Vec<Box<Bin>> = vec![
      box bins::Sprunge::new(config.clone(), cli_options.clone()),
      box bins::Hastebin::new(config.clone(), cli_options.clone()),
      box bins::Gist::new(config.clone(), cli_options.clone())
    ];
    bins.into_iter().map(|b| (b.name().to_owned(), b)).collect()
  };

  let b = Bins {
    bins: bins,
    config: config,
    cli_options: cli_options,
    matches: matches
  };

  if let Err(e) = b.main() {
    report_error!("error: {}", e);
    1
  } else {
    0
  }
}

struct Bins<'a> {
  bins: BTreeMap<String, Box<Bin>>,
  config: Arc<Config>,
  cli_options: Arc<CommandLineOptions>,
  matches: ArgMatches<'a>
}

impl<'a> Bins<'a> {
  fn main(&self) -> Result<()> {
    if self.matches.is_present("list-bins") {
      self.list_bins();
      return Ok(()); // FIXME
    }
    let inputs = self.raw_inputs();
    if let Some(ref is) = inputs {
      if !is.is_empty() {
        let url: Result<Url> = Url::parse(is[0]).map_err(BinsError::UrlParse);
        if let Ok(u) = url {
          self.download(u, if is.len() > 1 { Some(&is[1..]) } else { None }); // FIXME
          return Ok(());
        }
      }
    }
    self.upload(inputs)
  }

  fn raw_inputs(&self) -> Option<Vec<&str>> {
    self.matches.values_of("inputs").map(|x| x.collect())
  }

  fn list_bins(&self) -> i32 {
    if let Some(true) = self.cli_options.json {
      let names: Vec<&String> = self.bins.keys().collect();
      match serde_json::to_string(&names) {
        Ok(j) => println!("{}", j),
        Err(e) => {
          report_error!("error while encoding JSON: {}", e);
          return 1;
        }
      }
    } else {
      for name in self.bins.keys() {
        println!("{}", name);
      }
    }
    0
  }

  fn cli_features(&self) -> HashMap<BinFeature, Option<bool>> {
    let mut map = HashMap::new();
    map.insert(BinFeature::Private, self.cli_options.private);
    map.insert(BinFeature::Public, self.cli_options.private.map(|x| !x));
    map.insert(BinFeature::Authed, self.cli_options.authed);
    map.insert(BinFeature::Anonymous, self.cli_options.authed.map(|x| !x));
    map
  }

  fn bin_name(&self) -> Result<String> {
    self.matches.value_of("bin")
      .map(|x| x.to_owned())
      .or_else(|| self.config.defaults.bin.clone())
      .ok_or_else(|| BinsError::Main(MainError::NoBinSpecified))
  }

  fn bin(&self) -> Result<&Box<Bin>> {
    let name = self.bin_name()?;
    self.bins.get(&name).ok_or_else(|| BinsError::Main(MainError::NoSuchBin(name)))
  }

  fn check_features(&self, bin: &Box<Bin>) -> Result<()> {
    let bin_features = bin.features();
    let features = self.cli_features();
    for (feature, status) in features {
      if let Some(true) = status {
        if !bin_features.contains(&feature) {
          if let Some(true) = self.config.safety.warn_on_unsupported {
            warn!("{} does not support {} pastes", bin.name(), feature);
          }
          if let Some(true) = self.config.safety.cancel_on_unsupported {
            return match self.cli_options.force {
              Some(true) => {
                warn!("forcing upload with unsupported features");
                Ok(())
              },
              _ => Err(BinsError::Main(MainError::UnsupportedFeature(bin.name().to_owned(), feature)))
            }
          }
        }
      }
    }
    Ok(())
  }

  fn inputs(&self, inputs: Option<Vec<&str>>) -> Result<Vec<UploadFile>> {
    match inputs {
      Some(v) => get_upload_files(v),
      None => {
        if let Some(message) = self.matches.value_of("message") {
          Ok(vec![UploadFile::new(String::from("message"), message.to_owned())])
        } else {
          get_stdin().map(|x| vec![x])
        }
      }
    }
  }

  fn url_output(&self, bin: &Box<Bin>, urls: &[PasteUrl]) -> Result<()> {
    for u in urls {
      let id = bin.id_from_html_url(u.url()).ok_or_else(|| BinsError::Main(MainError::ParseId))?;
      let raw_urls = match bin.format_raw_url(&id) {
        Some(u) => vec![u],
        None => {
          let raw_url = bin.create_raw_url(&id)?;
          raw_url.into_iter().map(|x| x.url().to_owned()).collect()
        }
      };
      for raw_url in raw_urls {
        println!("{}", raw_url);
      }
    }
    Ok(())
  }

  fn upload(&self, inputs: Option<Vec<&str>>) -> Result<()> {
    let bin = self.bin()?;
    self.check_features(bin)?;

    let upload_files = self.inputs(inputs)?;
    #[cfg(feature = "file_type_checking")]
    self.check_file_types(&upload_files)?;
    let urls = bin.upload(&upload_files, self.cli_options.url_output.is_none())?;
    if let Some(UrlOutputMode::Raw) = self.cli_options.url_output {
      return self.url_output(bin, &urls);
    }
    for url in urls {
      println!("{}", url.url());
    }
    Ok(())
  }

  #[cfg(feature = "file_type_checking")]
  fn check_file_types(&self, files: &[UploadFile]) -> Result<()> {

    use magic::{Cookie, flags};

    let cookie = Cookie::open(flags::NONE).map_err(BinsError::Magic)?;
    cookie.load(&[""; 0]).map_err(BinsError::Magic)?;
    for upload_file in files {
      let kind = cookie.buffer(upload_file.content.as_bytes()).map_err(BinsError::Magic)?;
      if let Some(ref disallowed) = self.config.safety.disallowed_file_types {
        if disallowed.contains(&kind) {
          return match self.cli_options.force {
            Some(true) => {
              warn!("forcing upload with disallowed file type: ({} is {}, which is disallowed)", upload_file.name, kind);
              Ok(())
            },
            _ => Err(BinsError::InvalidFileType {
              name: upload_file.name.clone(),
              kind: kind
            })
          }
        }
      }
    }
    Ok(())
  }

  fn download(&self, url: Url, names: Option<&[&str]>) -> i32 {
    let host = match url.host_str() {
      Some(h) => h,
      None => {
        error!("invalid url (no host): {}", url.as_str());
        return 1;
      }
    };
    let (is_html_url, bin) = match self.bins.iter().find(|&(_, b)| b.raw_host() == host) {
      Some(b) => (false, b.1),
      None => {
        match self.bins.iter().find(|&(_, b)| b.html_host() == host) {
          Some(b) => (true, b.1),
          None => {
            error!("no bin uses the hostname {}", host);
            return 1;
          }
        }
      }
    };
    let id = if is_html_url {
      bin.id_from_html_url(url.as_str())
    } else {
      bin.id_from_raw_url(url.as_str())
    };
    let id = match id {
      Some(i) => i,
      None => {
        error!("could not extract paste ID from {}", url.as_str());
        return 1;
      }
    };
    if let Some(ref output_mode) = self.cli_options.url_output {
      let urls = match *output_mode {
        UrlOutputMode::Html => bin.create_html_url(&id),
        UrlOutputMode::Raw =>bin.create_raw_url(&id)
      };
      let urls = match urls {
        Ok(us) => us,
        Err(e) => {
          report_error!("error creating URLs from ID: {}", e);
          return 1;
        }
      };
      for url in urls {
        println!("{}", url.url());
      }
      return 0;
    }
    let download = match bin.download(&id, names) {
      Ok(d) => d,
      Err(e) => {
        report_error!("could not download ID {1}: {0}", e, id);
        return 1;
      }
    };
    if let Some(true) = self.cli_options.json {
      match serde_json::to_string(&download) {
        Ok(j) => println!("{}", j),
        Err(e) => {
          report_error!("error converting download to json: {}", e);
          return 1;
        }
      }
    } else {
      match download {
        Paste::Single(f) => {
          println!("{}", f.content);
        },
        Paste::Multiple(fs) => {
          for f in fs {
            println!("==> {} <==\n\n{}", f.name.name(), f.content);
          }
        }
      }
    }
    0
  }
}

fn get_stdin() -> Result<UploadFile> {
  let mut content = String::new();
  let mut stdin = std::io::stdin();
  stdin.read_to_string(&mut content).map_err(BinsError::Io)?;
  Ok(UploadFile::new("stdin".to_owned(), content))
}

fn get_upload_files(inputs: Vec<&str>) -> Result<Vec<UploadFile>> {
  let files: Option<Vec<(&str, File)>> = inputs.into_iter()
    .map(|f| File::open(f).map(|x| Path::new(f).file_name().and_then(|f| f.to_str()).map(|of| (of, x))))
    .collect::<IoResult<_>>()
    .map_err(BinsError::Io)?;
  let files = match files {
    Some(f) => f,
    None => {
      error!("one or more inputs did not have a file name or did not have a valid utf-8 file name");
      return Err(BinsError::Other);
    }
  };
  let contents: Vec<(&str, String)> = files.into_iter()
    .map(|(n, mut f)| {
      let mut c = String::new();
      f.read_to_string(&mut c).map(|_| (n, c))
    })
    .collect::<IoResult<_>>()
    .map_err(BinsError::Io)?;
  Ok(contents.into_iter().map(|(n, c)| UploadFile::new(n.to_owned(), c)).collect())
}

fn error_parents(error: &Error) -> Vec<&Error> {
  let mut parents = Vec::new();
  let mut last_error = error;
  loop {
    match last_error.cause() {
      None => break,
      Some(e) => {
        parents.push(e);
        last_error = e;
      }
    }
  }
  parents
}

fn get_config() -> Result<Config> {
  let mut f = match find_config_path() {
    Some(p) => File::open(p).map_err(BinsError::Io)?,
    None => create_config_file()?
  };
  let mut content = String::new();
  f.read_to_string(&mut content).map_err(BinsError::Io)?;
  toml::from_str(&content).map_err(BinsError::Toml)
}

fn create_xdg_config_file() -> Result<File> {
  if let Ok(xdg_dir) = std::env::var("XDG_CONFIG_DIR") {
    let xdg_path = Path::new(&xdg_dir);
    let xdg_config_path = xdg_path.join("bins.cfg");
    if xdg_path.exists() && xdg_path.is_dir() && !xdg_config_path.exists() {
      return OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(xdg_config_path)
        .map_err(BinsError::Io);
    }
  }
  Err(BinsError::Config)
}

fn create_home_config_file() -> Result<File> {
  if let Ok(home_dir) = std::env::var("HOME") {
    let home = Path::new(&home_dir);
    let home_folder = home.join(".config");
    let home_folder_config = home_folder.join("bins.cfg");
    if home_folder.exists() && home_folder.is_dir() && !home_folder_config.exists() {
      return OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(home_folder_config)
        .map_err(BinsError::Io);
    }
    let home_config = Path::new(&home_dir).join(".bins.cfg");
    if home.exists() && home.is_dir() && !home_config.exists() {
      return OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(home_config)
        .map_err(BinsError::Io);
    }
  }
  Err(BinsError::Config)
}

fn create_config_file() -> Result<File> {
  let mut f = match create_xdg_config_file() {
    Ok(f) => f,
    Err(_) => match create_home_config_file() {
      Ok(hf) => hf,
      Err(_) => return Err(BinsError::Config)
    }
  };
  let mut default_config = String::new();
  GzDecoder::new(config::DEFAULT_CONFIG_GZIP)
    .map_err(BinsError::Io)?
    .read_to_string(&mut default_config)
    .map_err(BinsError::Io)?;
  f.write_all(default_config.as_bytes()).map_err(BinsError::Io)?;
  f.seek(SeekFrom::Start(0)).map_err(BinsError::Io)?;
  Ok(f)
}

fn find_config_path() -> Option<PathBuf> {
  if let Ok(xdg_dir) = std::env::var("XDG_CONFIG_DIR") {
    let xdg_config_path = Path::new(&xdg_dir).join("bins.cfg");
    if xdg_config_path.exists() {
      return Some(xdg_config_path.to_owned());
    }
  }
  if let Ok(home_dir) = std::env::var("HOME") {
    let home_config_folder = Path::new(&home_dir).join(".config").join("bins.cfg");
    if home_config_folder.exists() {
      return Some(home_config_folder.to_owned());
    }
    let home_config = Path::new(&home_dir).join(".bins.cfg");
    if home_config.exists() {
      return Some(home_config.to_owned());
    }
  }
  None
}
