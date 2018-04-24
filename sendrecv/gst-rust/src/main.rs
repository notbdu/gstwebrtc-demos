extern crate ws;
extern crate rand;
extern crate clap;
extern crate url;
extern crate openssl;
extern crate gstreamer as gst;

use std::process;

use rand::{Rng, thread_rng};
use ws::{
    connect,
    Handler,
    Sender,
    Handshake,
    Result,
    Message,
    CloseCode,
    Error,
    Frame,
    Response,
    Request,
	ErrorKind,
};
use ws::util::{Token, TcpStream};
use openssl::ssl::{SslVerifyMode, SslStream, SslConnectorBuilder, SslMethod};
use clap::{App, Arg};
use gst::{Cast, BinExt, ElementExt};

const STUN_SERVER: &'static str = "stun-server=stun://stun.l.google.com:19302";
const RTP_CAPS_OPUS: &'static str = "application/x-rtp,media=audio,encoding-name=OPUS,payload=";
const RTP_CAPS_VP8: &'static str = "application/x-rtp,media=video,encoding-name=VP8,payload=";

#[derive(Debug, PartialEq)]
enum AppState {
    Error,
    ServerConnecting,
    ServerConnected,
    ServerRegistering,
    ServerRegistrationError,
    ServerRegistered,
    ServerClosed,
    PeerConnecting,
    PeerConnectionError,
    PeerConnected,
    PeerCallNegotiating,
    PeerCallStarted,
    PeerCallStopping,
    PeerCallStopped,
    PeerCallError,
}

struct Client<'a> {
    out: Sender,
    state: AppState,
    peer_id: &'a str,
}

// We implement the Handler trait for Client so that we can get more
// fine-grained control of the connection.
impl<'a> Handler for Client<'a> {

    fn on_open(&mut self, _: Handshake) -> Result<()> {
        self.state = AppState::ServerConnected;

        // Register w/ the signalling server
        let id = thread_rng().gen_range(10, 10000);
        println!("Registering id {} w/ server", id);
        self.state = AppState::ServerRegistering;
        self.out.send(format!("HELLO {}", id))
    }

    #[inline]
    fn on_message(&mut self, msg: Message) -> Result<()> {
        // Close the connection when we get a response from the server
        println!("Got message: {}", msg);
        match msg.into_text().unwrap().as_ref() {
            "HELLO" => {
                if self.state != AppState::ServerRegistering {
                    self.state = AppState::Error;
                    Err(Error::new(
                        ErrorKind::Internal,
                        "Error: Received HELLO when not registering",
                    ))
                } else {
                    self.state = AppState::ServerRegistered;
                    println!("Server registered");
                    self.state = AppState::PeerConnecting;
                    self.out.send(format!("SESSION {}", self.peer_id))
                }
            },
            "SESSION_OK" => {
                if self.state != AppState::PeerConnecting {
                    self.state = AppState::Error;
                    Err(Error::new(
                        ErrorKind::Internal,
                        "Error: Received SESSION_OK when not bootstrapping session",
                    ))
                } else {
                    self.state = AppState::PeerConnected;
                    // TODO: Start gstreamer pipeline now
                    let pipeline =
                        match gst::parse_launch(format!(
                            "webrtcbin name=sendrecv {stun_server} \
                            videotestsrc pattern=ball ! videoconvert ! queue ! vp8enc deadline=1 ! rtpvp8pay ! \
                            queue ! {rtp_caps_vp8}96 ! sendrecv. \
                            audiotestsrc wave=red-noise ! audioconvert ! audioresample ! queue ! opusenc ! rtpopuspay ! \
                            queue !  {rtp_caps_opus}97 ! sendrecv.",
                            stun_server = STUN_SERVER,
                            rtp_caps_vp8 = RTP_CAPS_VP8,
                            rtp_caps_opus = RTP_CAPS_OPUS,
                        ).as_ref()) {
                            Ok(pipeline) => pipeline,
                            Err(err) => {
                                println!("Failed to parse pipeline: {}", err);
                                process::exit(-1)
                            },
                        };

                    // Get webrtc sink
                    let webrtc = pipeline
                        .clone()
                        .dynamic_cast::<gst::Bin>()
                        .unwrap()
                        .get_by_name("sendrecv")
                        .unwrap();

                    // TODO: Connect webrtc callbacks
                    let ret = pipeline.set_state(gst::State::Playing);
                    assert_ne!(ret, gst::StateChangeReturn::Failure);

                    Ok(())
                }
            },
            _ => Err(Error::new(
                ErrorKind::Internal,
                "Error: Received an unhandled message",
            )),
        }
    }

    fn on_error(&mut self, err: Error) {
        println!("got error: {}", err);
    }

    fn on_shutdown(&mut self) {
        println!("Handler received WebSocket shutdown request.");
    }

    fn upgrade_ssl_client(
        &mut self,
        stream: TcpStream,
        url: &url::Url,
    ) -> Result<SslStream<TcpStream>> {
        let domain = url.domain().ok_or(Error::new(
            ErrorKind::Protocol,
            format!("Unable to parse domain from {}. Needed for SSL.", url),
        ))?;
        let mut builder = SslConnectorBuilder::new(SslMethod::tls())
            .map_err(|e| {
                Error::new(
                    ErrorKind::Internal,
                    format!("Failed to upgrade client to SSL: {}", e),
                )
            })?;

        // Do not verify ssl certs if we are serving locally
        let host = url.host_str().unwrap();
        if host == "localhost" || host == "127.0.0.1" {
            builder.set_verify(SslVerifyMode::empty());
        }

        let connector = builder.build();

        connector.connect(domain, stream).map_err(Error::from)
    }
}

fn check_plugins() {
    let required = vec![
        "opus",
        "vpx",
        "nice",
        "webrtc",
        "dtls",
        "srtp",
        "rtpmanager",
        "videotestsrc",
        "audiotestsrc",
    ];

    let registry = gst::Registry::get();
    for plugin in required.iter() {
        match registry.find_plugin(plugin) {
            Some(_) => (),
            None => {
                println!("required gstreamer plugin {} not found", plugin);
                process::exit(1);
            },
        }
    }
}

fn main() {
    // Parse arguments
   let matches = App::new("webrtc-sendrecv")
       .about("Gstreamer webrtc sendrecv demo")
       .arg(Arg::with_name("peer-id")
            .long("peer-id")
            .value_name("ID")
            .help("String ID of the peer to connect to")
            .takes_value(true)
            .required(true))
       .arg(Arg::with_name("server")
            .long("server")
            .value_name("URL")
            .help("Signalling server to connect to")
            .default_value("wss://webrtc.nirbheek.in:8443")
            .takes_value(true))
       .get_matches();

    let peer_id = matches.value_of("peer-id").unwrap();
    let server = matches.value_of("server").unwrap();

    // initialize gstreamer
    gst::init().unwrap();

    check_plugins();

    println!("Connecting to server: {}", server);
	match connect(server, |out| Client {
        out: out,
        state: AppState::ServerConnecting,
        peer_id: peer_id,
    }) {
        Ok(()) => println!("connect success"),
        Err(err) => println!("connect error: {}", err),
    }
    loop {
    }
}
