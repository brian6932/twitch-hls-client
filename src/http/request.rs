use std::{
    io::{
        self, BufRead, BufReader,
        ErrorKind::{InvalidInput, Other, UnexpectedEof},
        Read, Write,
    },
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    sync::Arc,
};

use anyhow::{bail, ensure, Context, Result};
use log::{debug, error, info};
use rustls::{ClientConfig, ClientConnection, StreamOwned};

use super::{decoder::Decoder, Agent, Error, Url};

pub struct TextRequest {
    request: Request<StringWriter>,
}

impl TextRequest {
    pub fn new(request: Request<StringWriter>) -> Self {
        Self { request }
    }

    pub fn header(&mut self, header: &str) -> Result<()> {
        self.request.header(header)
    }

    pub fn text(&mut self) -> Result<&str> {
        self.request.get_mut().0.clear();
        self.request.call()?;

        Ok(&self.request.get_mut().0)
    }
}

pub struct WriterRequest<T>
where
    T: Write,
{
    request: Request<T>,
}

impl<T: Write> WriterRequest<T> {
    pub fn new(mut request: Request<T>) -> Result<Self> {
        request.call()?;
        Ok(Self { request })
    }

    pub fn call(&mut self, url: Url) -> Result<()> {
        self.request.url(url)?;
        self.request.call()
    }
}

pub struct Request<T>
where
    T: Write,
{
    stream: BufReader<Transport>,
    handler: Handler<T>,
    raw: String,

    method: Method,
    url: Url,
    headers: String,
    data: String,

    agent: Agent,
}

impl<T: Write> Request<T> {
    pub fn new(writer: T, method: Method, url: Url, data: String, agent: Agent) -> Result<Self> {
        let mut request = Self {
            stream: BufReader::new(Transport::new(&url, agent.clone())?),
            handler: Handler::new(writer),
            raw: String::default(),

            method,
            url,
            headers: String::default(),
            data,

            agent,
        };
        request.build()?;

        if !request.data.is_empty() {
            request.header(&format!("Content-Length: {}", request.data.len()))?;
        }

        Ok(request)
    }

    fn get_mut(&mut self) -> &mut T {
        self.handler.writer.as_mut().expect("Missing writer")
    }

    fn header(&mut self, header: &str) -> Result<()> {
        self.headers = format!(
            "{}\
             {header}\r\n",
            self.headers
        );

        self.build()
    }

    fn url(&mut self, url: Url) -> Result<()> {
        if self.url.scheme()? != url.scheme()? || self.url.host()? != url.host()? {
            return self.reconnect(url);
        }

        self.url = url;
        self.build()
    }

    fn call(&mut self) -> Result<()> {
        let mut retries = 0;
        loop {
            match self.do_request() {
                Ok(()) => break,
                Err(e) if retries < self.agent.args.retries => {
                    match e.downcast_ref::<io::Error>() {
                        Some(i) if matches!(i.kind(), Other) => return Err(e),
                        Some(_) => (),
                        _ => return Err(e),
                    }

                    error!("http: {e}");
                    retries += 1;

                    self.reconnect(self.url.clone())?;

                    let written = self.handler.written;
                    if written > 0 {
                        info!("Resuming from offset: {written} bytes");
                        self.handler.resume_target = written;
                        self.handler.written = 0;
                    }
                }
                Err(e) => return Err(e),
            }
        }

        self.handler.written = 0;
        self.handler
            .writer
            .as_mut()
            .expect("Missing writer")
            .flush()?;

        Ok(())
    }

    fn do_request(&mut self) -> Result<()> {
        //Will break if server sends more than this in headers, but protects against OOM
        const MAX_HEADERS_SIZE: usize = 2048;
        //Read only \r\n
        const HEADERS_END_SIZE: usize = 2;

        debug!("Request:\n{}", self.raw);
        self.stream.get_mut().write_all(self.raw.as_bytes())?;

        let mut response = Vec::new();
        let mut consumed = 0;
        while consumed != HEADERS_END_SIZE {
            if self.stream.fill_buf()?.is_empty() {
                return Err(io::Error::from(UnexpectedEof).into());
            }

            consumed = self
                .stream
                .by_ref()
                .take(MAX_HEADERS_SIZE as u64)
                .read_until(b'\n', &mut response)?;
        }

        let headers = String::from_utf8_lossy(&response);
        debug!("Response:\n{headers}");

        let code = headers
            .split_whitespace()
            .nth(1)
            .context("Failed to find request status code")?
            .parse()
            .context("Failed to parse request status code")?;

        match code {
            200 => (),
            404 => return Err(Error::NotFound(self.url.clone()).into()),
            _ => return Err(Error::Status(code, self.url.clone()).into()),
        }

        if let Err(e) = io::copy(
            &mut Decoder::new(&mut self.stream, &headers)?,
            &mut self.handler,
        ) {
            //Chunk decoder returns InvalidInput on some segment servers, can be ignored
            if !matches!(e.kind(), InvalidInput) {
                return Err(e.into());
            }
        }

        Ok(())
    }

    fn reconnect(&mut self, url: Url) -> Result<()> {
        debug!("Reconnecting...");
        *self = Request::new(
            self.handler.writer.take().expect("Missing writer"),
            self.method,
            url,
            self.data.clone(),
            self.agent.clone(),
        )?;

        Ok(())
    }

    fn build(&mut self) -> Result<()> {
        let method = match self.method {
            Method::Get => "GET",
            Method::Post => "POST",
        };

        let headers = format!(
            "{method} /{path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             User-Agent: {user_agent}\r\n\
             Accept: */*\r\n\
             Accept-Language: en-US\r\n\
             Accept-Encoding: gzip\r\n\
             Connection: keep-alive\r\n\
             {headers}",
            path = self.url.path()?,
            host = self.url.host()?,
            user_agent = &self.agent.args.user_agent,
            headers = self.headers
        );

        self.raw = format!("{}\r\n{}", headers, self.data);
        Ok(())
    }
}

#[derive(Copy, Clone)]
pub enum Method {
    Get,
    Post,
}

#[allow(clippy::large_enum_variant)]
pub enum Transport {
    Http(TcpStream),
    Https(StreamOwned<ClientConnection, TcpStream>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Http(sock) => sock.read(buf),
            Self::Https(stream) => stream.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Http(sock) => sock.write(buf),
            Self::Https(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Http(sock) => sock.flush(),
            Self::Https(stream) => stream.flush(),
        }
    }
}

impl Transport {
    pub fn new(url: &Url, agent: Agent) -> Result<Self> {
        let scheme = url.scheme()?;
        let host = url.host()?;
        let port = url.port()?;

        if agent.args.force_https {
            ensure!(
                scheme == "https",
                "URL protocol is not HTTPS and --force-https is enabled: {url}",
            );
        }

        let addr = format!("{host}:{port}");
        let sock = if agent.args.force_ipv4 {
            TcpStream::connect(
                &*addr
                    .to_socket_addrs()?
                    .filter(SocketAddr::is_ipv4)
                    .collect::<Vec<_>>(),
            )?
        } else {
            TcpStream::connect(addr)?
        };

        sock.set_nodelay(true)?;
        sock.set_read_timeout(Some(agent.args.timeout))?;
        sock.set_write_timeout(Some(agent.args.timeout))?;

        match scheme {
            "http" => Ok(Self::Http(sock)),
            "https" => Ok(Self::Https(Self::init_tls(host, sock, agent.tls_config)?)),
            _ => bail!("{scheme} is not supported"),
        }
    }

    fn init_tls(
        host: &str,
        mut sock: TcpStream,
        tls_config: Arc<ClientConfig>,
    ) -> Result<StreamOwned<ClientConnection, TcpStream>> {
        let mut conn = ClientConnection::new(tls_config, host.to_owned().try_into()?)?;
        conn.complete_io(&mut sock)?; //handshake

        Ok(StreamOwned::new(conn, sock))
    }
}

#[derive(Default)]
pub struct StringWriter(String);

impl Write for StringWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        unimplemented!();
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.0.push_str(&String::from_utf8_lossy(buf));
        Ok(())
    }
}

struct Handler<T>
where
    T: Write,
{
    writer: Option<T>,

    written: usize,
    resume_target: usize,
}

impl<T: Write> Write for Handler<T> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let buf_len = buf.len();
        if self.resume_target > 0 {
            if (self.written + buf_len) >= self.resume_target {
                buf = &buf[self.resume_target - self.written..];
                self.resume_target = 0;
            } else {
                self.written += buf_len;
                return Ok(buf_len); //throw buf into the void
            }
        }

        self.writer
            .as_mut()
            .expect("Missing writer")
            .write_all(buf)?;

        self.written += buf.len(); //len of the potential trimmed buf reference
        Ok(buf_len)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<T: Write> Handler<T> {
    fn new(writer: T) -> Self {
        Self {
            writer: Some(writer),

            written: usize::default(),
            resume_target: usize::default(),
        }
    }
}