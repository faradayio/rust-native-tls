extern crate libc;
extern crate security_framework;
extern crate security_framework_sys;
extern crate tempfile;

use self::security_framework::base;
use self::security_framework::certificate::SecCertificate;
use self::security_framework::identity::SecIdentity;
use self::security_framework::import_export::{ImportedIdentity, Pkcs12ImportOptions};
use self::security_framework::secure_transport::{
    self, ClientBuilder, SslConnectionType, SslContext, SslProtocol, SslProtocolSide,
};
use self::security_framework_sys::base::errSecIO;
use self::tempfile::TempDir;
use std::error;
use std::fmt;
use std::io;
use std::sync::Mutex;
use std::sync::{Once, ONCE_INIT};

#[cfg(not(target_os = "ios"))]
use self::security_framework::os::macos::import_export::{ImportOptions, SecItems};
#[cfg(not(target_os = "ios"))]
use self::security_framework::os::macos::keychain::{self, KeychainSettings, SecKeychain};
#[cfg(not(target_os = "ios"))]
use self::security_framework_sys::base::errSecParam;

use Protocol;

static SET_AT_EXIT: Once = ONCE_INIT;

#[cfg(not(target_os = "ios"))]
lazy_static! {
    static ref TEMP_KEYCHAIN: Mutex<Option<(SecKeychain, TempDir)>> = Mutex::new(None);
}

fn convert_protocol(protocol: Protocol) -> SslProtocol {
    match protocol {
        Protocol::Sslv3 => SslProtocol::SSL3,
        Protocol::Tlsv10 => SslProtocol::TLS1,
        Protocol::Tlsv11 => SslProtocol::TLS11,
        Protocol::Tlsv12 => SslProtocol::TLS12,
        Protocol::__NonExhaustive => unreachable!(),
    }
}

fn protocol_min_max(protocols: &[Protocol]) -> (SslProtocol, SslProtocol) {
    let mut min = Protocol::Tlsv12;
    let mut max = Protocol::Sslv3;
    for protocol in protocols {
        if (*protocol as usize) < (min as usize) {
            min = *protocol;
        }
        if (*protocol as usize) > (max as usize) {
            max = *protocol;
        }
    }
    (convert_protocol(min), convert_protocol(max))
}

pub struct Error(base::Error);

impl error::Error for Error {
    fn description(&self) -> &str {
        error::Error::description(&self.0)
    }

    fn cause(&self) -> Option<&error::Error> {
        error::Error::cause(&self.0)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, fmt)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, fmt)
    }
}

impl From<base::Error> for Error {
    fn from(error: base::Error) -> Error {
        Error(error)
    }
}

#[derive(Clone)]
pub struct Identity {
    identity: SecIdentity,
    chain: Vec<SecCertificate>,
}

impl Identity {
    pub fn from_pkcs12(buf: &[u8], pass: &str) -> Result<Identity, Error> {
        let mut imports = Identity::import_options(buf, pass)?;
        let import = imports.pop().unwrap();

        let identity = import
            .identity
            .expect("Pkcs12 files must include an identity");

        // FIXME: Compare the certificates for equality using CFEqual
        let identity_cert = identity.certificate()?.to_der();

        Ok(Identity {
            identity: identity,
            chain: import
                .cert_chain
                .unwrap_or(vec![])
                .into_iter()
                .filter(|c| c.to_der() != identity_cert)
                .collect(),
        })
    }

    #[cfg(not(target_os = "ios"))]
    fn import_options(buf: &[u8], pass: &str) -> Result<Vec<ImportedIdentity>, Error> {
        SET_AT_EXIT.call_once(|| {
            extern "C" fn atexit() {
                *TEMP_KEYCHAIN.lock().unwrap() = None;
            }
            unsafe {
                libc::atexit(atexit);
            }
        });

        let keychain = match *TEMP_KEYCHAIN.lock().unwrap() {
            Some((ref keychain, _)) => keychain.clone(),
            ref mut lock @ None => {
                let dir = TempDir::new().map_err(|_| Error(base::Error::from(errSecIO)))?;

                let mut keychain = keychain::CreateOptions::new()
                    .password(pass)
                    .create(dir.path().join("tmp.keychain"))?;
                keychain.set_settings(&KeychainSettings::new())?;

                *lock = Some((keychain.clone(), dir));
                keychain
            }
        };
        let imports = Pkcs12ImportOptions::new()
            .passphrase(pass)
            .keychain(keychain)
            .import(buf)?;
        Ok(imports)
    }

    #[cfg(target_os = "ios")]
    fn import_options(buf: &[u8], pass: &str) -> Result<Vec<ImportedIdentity>, Error> {
        let imports = Pkcs12ImportOptions::new().passphrase(pass).import(buf)?;
        Ok(imports)
    }
}

#[derive(Clone)]
pub struct Certificate(SecCertificate);

impl Certificate {
    pub fn from_der(buf: &[u8]) -> Result<Certificate, Error> {
        let cert = SecCertificate::from_der(buf)?;
        Ok(Certificate(cert))
    }

    #[cfg(not(target_os = "ios"))]
    pub fn from_pem(buf: &[u8]) -> Result<Certificate, Error> {
        let mut items = SecItems::default();
        ImportOptions::new().items(&mut items).import(buf)?;
        match items.certificates.pop() {
            Some(cert) => Ok(Certificate(cert)),
            None => Err(Error(base::Error::from(errSecParam))),
        }
    }
    #[cfg(target_os = "ios")]
    pub fn from_pem(buf: &[u8]) -> Result<Certificate, Error> {
        panic!("Not implemented on iOS");
    }
}

pub enum HandshakeError<S> {
    WouldBlock(MidHandshakeTlsStream<S>),
    Failure(Error),
}

impl<S> From<secure_transport::HandshakeError<S>> for HandshakeError<S> {
    fn from(e: secure_transport::HandshakeError<S>) -> HandshakeError<S> {
        match e {
            secure_transport::HandshakeError::Failure(e) => HandshakeError::Failure(e.into()),
            secure_transport::HandshakeError::Interrupted(s) => {
                HandshakeError::WouldBlock(MidHandshakeTlsStream::Server(s))
            }
        }
    }
}

impl<S> From<secure_transport::ClientHandshakeError<S>> for HandshakeError<S> {
    fn from(e: secure_transport::ClientHandshakeError<S>) -> HandshakeError<S> {
        match e {
            secure_transport::ClientHandshakeError::Failure(e) => HandshakeError::Failure(e.into()),
            secure_transport::ClientHandshakeError::Interrupted(s) => {
                HandshakeError::WouldBlock(MidHandshakeTlsStream::Client(s))
            }
        }
    }
}

impl<S> From<base::Error> for HandshakeError<S> {
    fn from(e: base::Error) -> HandshakeError<S> {
        HandshakeError::Failure(e.into())
    }
}

pub enum MidHandshakeTlsStream<S> {
    Server(secure_transport::MidHandshakeSslStream<S>),
    Client(secure_transport::MidHandshakeClientBuilder<S>),
}

impl<S> fmt::Debug for MidHandshakeTlsStream<S>
where
    S: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            MidHandshakeTlsStream::Server(ref s) => s.fmt(fmt),
            MidHandshakeTlsStream::Client(ref s) => s.fmt(fmt),
        }
    }
}

impl<S> MidHandshakeTlsStream<S>
where
    S: io::Read + io::Write,
{
    pub fn get_ref(&self) -> &S {
        match *self {
            MidHandshakeTlsStream::Server(ref s) => s.get_ref(),
            MidHandshakeTlsStream::Client(ref s) => s.get_ref(),
        }
    }

    pub fn get_mut(&mut self) -> &mut S {
        match *self {
            MidHandshakeTlsStream::Server(ref mut s) => s.get_mut(),
            MidHandshakeTlsStream::Client(ref mut s) => s.get_mut(),
        }
    }

    pub fn handshake(self) -> Result<TlsStream<S>, HandshakeError<S>> {
        match self {
            MidHandshakeTlsStream::Server(s) => match s.handshake() {
                Ok(s) => Ok(TlsStream(s)),
                Err(e) => Err(e.into()),
            },
            MidHandshakeTlsStream::Client(s) => match s.handshake() {
                Ok(s) => Ok(TlsStream(s)),
                Err(e) => Err(e.into()),
            },
        }
    }
}

pub struct TlsConnectorBuilder(TlsConnector);

impl TlsConnectorBuilder {
    pub fn identity(&mut self, identity: Identity) -> Result<(), Error> {
        self.0.identity = Some(identity);
        Ok(())
    }

    pub fn add_root_certificate(&mut self, cert: Certificate) -> Result<(), Error> {
        self.0.roots.push(cert.0);
        Ok(())
    }

    pub fn use_sni(&mut self, use_sni: bool) {
        self.0.use_sni = use_sni;
    }

    pub fn danger_accept_invalid_hostnames(&mut self, accept_invalid_hostnames: bool) {
        self.0.danger_accept_invalid_hostnames = accept_invalid_hostnames;
    }

    pub fn danger_accept_invalid_certs(&mut self, accept_invalid_certs: bool) {
        self.0.danger_accept_invalid_certs = accept_invalid_certs;
    }

    pub fn supported_protocols(&mut self, protocols: &[Protocol]) -> Result<(), Error> {
        self.0.protocols = protocols.to_vec();
        Ok(())
    }

    pub fn build(self) -> Result<TlsConnector, Error> {
        Ok(self.0)
    }
}

#[derive(Clone)]
pub struct TlsConnector {
    identity: Option<Identity>,
    protocols: Vec<Protocol>,
    roots: Vec<SecCertificate>,
    use_sni: bool,
    danger_accept_invalid_hostnames: bool,
    danger_accept_invalid_certs: bool,
}

impl TlsConnector {
    pub fn builder() -> Result<TlsConnectorBuilder, Error> {
        Ok(TlsConnectorBuilder(TlsConnector {
            identity: None,
            protocols: vec![Protocol::Tlsv10, Protocol::Tlsv11, Protocol::Tlsv12],
            roots: vec![],
            use_sni: true,
            danger_accept_invalid_hostnames: false,
            danger_accept_invalid_certs: false,
        }))
    }

    pub fn connect<S>(&self, domain: &str, stream: S) -> Result<TlsStream<S>, HandshakeError<S>>
    where
        S: io::Read + io::Write,
    {
        let mut builder = ClientBuilder::new();
        let (min, max) = protocol_min_max(&self.protocols);
        builder.protocol_min(min);
        builder.protocol_max(max);
        if let Some(identity) = self.identity.as_ref() {
            builder.identity(&identity.identity, &identity.chain);
        }
        builder.anchor_certificates(&self.roots);
        builder.use_sni(self.use_sni);
        builder.danger_accept_invalid_hostnames(self.danger_accept_invalid_hostnames);
        builder.danger_accept_invalid_certs(self.danger_accept_invalid_certs);

        match builder.handshake(domain, stream) {
            Ok(s) => Ok(TlsStream(s)),
            Err(e) => Err(e.into()),
        }
    }
}

pub struct TlsAcceptorBuilder(TlsAcceptor);

impl TlsAcceptorBuilder {
    pub fn supported_protocols(&mut self, protocols: &[Protocol]) -> Result<(), Error> {
        self.0.protocols = protocols.to_vec();
        Ok(())
    }

    pub fn build(self) -> Result<TlsAcceptor, Error> {
        Ok(self.0)
    }
}

#[derive(Clone)]
pub struct TlsAcceptor {
    identity: Identity,
    protocols: Vec<Protocol>,
}

impl TlsAcceptor {
    pub fn builder(identity: Identity) -> Result<TlsAcceptorBuilder, Error> {
        Ok(TlsAcceptorBuilder(TlsAcceptor {
            identity,
            protocols: vec![Protocol::Tlsv10, Protocol::Tlsv11, Protocol::Tlsv12],
        }))
    }

    pub fn accept<S>(&self, stream: S) -> Result<TlsStream<S>, HandshakeError<S>>
    where
        S: io::Read + io::Write,
    {
        let mut ctx = SslContext::new(SslProtocolSide::SERVER, SslConnectionType::STREAM)?;

        let (min, max) = protocol_min_max(&self.protocols);
        ctx.set_protocol_version_min(min)?;
        ctx.set_protocol_version_max(max)?;
        ctx.set_certificate(&self.identity.identity, &self.identity.chain)?;
        match ctx.handshake(stream) {
            Ok(s) => Ok(TlsStream(s)),
            Err(e) => Err(e.into()),
        }
    }
}

pub struct TlsStream<S>(secure_transport::SslStream<S>);

impl<S: fmt::Debug> fmt::Debug for TlsStream<S> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, fmt)
    }
}

impl<S: io::Read + io::Write> TlsStream<S> {
    pub fn get_ref(&self) -> &S {
        self.0.get_ref()
    }

    pub fn get_mut(&mut self) -> &mut S {
        self.0.get_mut()
    }

    pub fn buffered_read_size(&self) -> Result<usize, Error> {
        Ok(self.0.context().buffered_read_size()?)
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        self.0.close()?;
        Ok(())
    }
}

impl<S: io::Read + io::Write> io::Read for TlsStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<S: io::Read + io::Write> io::Write for TlsStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
