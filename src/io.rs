use std::cmp::min;
use consts::{ZkError, ZkState};
use proto::{ByteBuf, ConnectRequest, ConnectResponse, OpCode, ReadFrom, ReplyHeader, RequestHeader, SetWatchesRequest, to_len_prefixed_buf, WriteTo};
use watch::WatchMessage;
use zookeeper::{RawResponse, RawRequest};
use listeners::ListenerSet;

use byteorder::{BigEndian, ByteOrder};
use bytes::{Buf, Bytes, BytesMut};
use mio::net::TcpStream;
use mio::*;
use mio_extras::channel::{Sender, Receiver, channel};
use mio_extras::timer::{Timer, Timeout};
use std::collections::VecDeque;
use std::io;
use std::io::{Cursor, ErrorKind};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use std::sync::mpsc;
use std::mem;

const ZK: Token = Token(1);
const TIMER: Token = Token(2);
const CHANNEL: Token = Token(3);
const SESSION_TIMER: Token = Token(4);

// Copy from the java
const NOTIFICATION_XID: i32 = -1;
const PING_XID: i32 = -2;
const AUTHPACKET_XID: i32 = -4;
const SET_WATCHES_XID: i32 = -8;

use try_io::{TryRead, TryWrite};

lazy_static! {
    static ref PING: ByteBuf =
    RequestHeader{xid: -2, opcode: OpCode::Ping}.to_len_prefixed_buf().unwrap();
}


struct Hosts {
    addrs: Vec<SocketAddr>,
    index: usize,
}

#[inline]
fn pollopt() -> PollOpt {
    PollOpt::edge() | PollOpt::oneshot()
}

impl Hosts {
    fn new(addrs: Vec<SocketAddr>) -> Hosts {
        Hosts {
            addrs: addrs,
            index: 0,
        }
    }

    fn get(&mut self) -> &SocketAddr {
        let addr = &self.addrs[self.index];
        if self.addrs.len() == self.index + 1 {
            self.index = 0;
        } else {
            self.index += 1;
        }
        addr
    }

    fn size(&self) -> usize {
        self.addrs.len()
    }
}

#[derive(Clone, Debug)]
enum ZkTimeout {
    Ping,
    Connect,
}

#[derive(Clone, Debug)]
enum ZkSessionTimeout {
    Session
}

pub struct ZkIo {
    sock: TcpStream,
    state: ZkState,
    hosts: Hosts,
    buffer: VecDeque<RawRequest>,
    inflight: VecDeque<RawRequest>,
    response: BytesMut,
    ping_timeout: Option<Timeout>,
    conn_timeout: Option<Timeout>,
    timer: Timer<ZkTimeout>,
    timeout_ms: u64,
    ping_timeout_duration: Duration,
    conn_timeout_duration: Duration,
    watch_sender: mpsc::Sender<WatchMessage>,
    conn_resp: ConnectResponse,
    zxid: i64,
    ping_sent: Instant,
    state_listeners: ListenerSet<ZkState>,
    poll: Poll,
    shutdown: bool,
    tx: Sender<RawRequest>,
    rx: Receiver<RawRequest>,
    // timeout and timer for session
    session_timeout: Option<Timeout>,
    session_timer: Timer<ZkSessionTimeout>,
    session_sent: Instant,
}
const MAX_INTERVAL_PING_MS : u64 = 20000;
const MIN_INTERVAL_CONNECT_SERVER_MS : u64 = 5000;

impl ZkIo {
    pub fn new(
        addrs: Vec<SocketAddr>,
        session_timeout_duration: Duration,
        watch_sender: mpsc::Sender<WatchMessage>,
        state_listeners: ListenerSet<ZkState>
    ) -> ZkIo {
        trace!("ZkIo::new");
        let timeout_ms = session_timeout_duration.as_secs() * 1000 +
            session_timeout_duration.subsec_nanos() as u64 / 1000000;
        let conn_timeout_duration = Duration::from_millis(timeout_ms / addrs.len() as u64);

        // max interval for ping is 10s
        let ping_timeout_duration = Duration::from_millis(min(timeout_ms / 3,
                                                              MAX_INTERVAL_PING_MS));
        info!("Init the io event:  expected session timeout {:?}, connect timeout {:?}, ping timeout {:?}",
            session_timeout_duration, conn_timeout_duration, ping_timeout_duration);
        if (timeout_ms / addrs.len() as u64) < MIN_INTERVAL_CONNECT_SERVER_MS {
            warn!("Init the io event, the connect timeout {:?} is smaller than {:?}",
                conn_timeout_duration, Duration::from_millis(MIN_INTERVAL_CONNECT_SERVER_MS));
        }
        let (tx, rx) = channel();

        let mut zkio = ZkIo {
            sock: TcpStream::connect(&addrs[0]).unwrap(), // TODO I need a socket here, sorry.
            state: ZkState::Connecting,
            hosts: Hosts::new(addrs),
            buffer: VecDeque::new(),
            inflight: VecDeque::new(),
            // TODO server reads max up to 1MB, otherwise drops the connection,
            // size should be 1MB + tcp rcvBufsize
            response: BytesMut::with_capacity(1024 * 1024 * 2),
            ping_timeout: None,
            conn_timeout: None,
            ping_timeout_duration,
            conn_timeout_duration,
            // conn_timeout_duration: Duration::from_secs(2),
            timeout_ms,
            watch_sender,
            conn_resp: ConnectResponse::initial(timeout_ms),
            zxid: 0,
            ping_sent: Instant::now(),
            state_listeners,
            // TODO add error handling to this method in subsequent commit.
            // There's already another unwrap which needs to be addressed.
            poll: Poll::new().unwrap(),
            shutdown: false,
            timer: Timer::default(),
            tx,
            rx,
            session_timeout: None,
            session_timer: Timer::default(),
            session_sent: Instant::now(),
        };

        let request = zkio.connect_request();
        zkio.buffer.push_back(request);
        zkio
    }

    fn reregister(&mut self, interest: Ready) {
        self.poll
            .reregister(&self.sock, ZK, interest, pollopt())
            .expect("Failed to register ZK handle");
    }

    fn notify_state(&self, old_state: ZkState, new_state: ZkState) {
        if new_state != old_state {
            self.state_listeners.notify(&new_state);
        }
    }

    fn handle_response(&mut self) {
        loop {
            if self.response.len() <= 4 {
                return;
            }

            let len = BigEndian::read_i32(&self.response[..4]) as usize;

            trace!("Response chunk len = {} buf len is {}", len, self.response.len());

            if self.response.len() - 4 < len {
                return;
            } else {
                self.response.advance(4);
                let bytes = self.response.split_to(len);
                self.handle_chunk(bytes.freeze());

                self.response.reserve(1024 * 1024 * 2);
            }
        }
    }

    fn handle_chunk(&mut self, bytes: Bytes) {
        let len = bytes.len();
        trace!("handle_response in {:?} state [{}]", self.state, len);

        let mut data = &*bytes;

        if self.state != ZkState::Connecting {
            let header = match ReplyHeader::read_from(&mut data) {
                Ok(header) => header,
                Err(e) => {
                    error!("Failed to parse ReplyHeader {:?}", e);
                    panic!("Failed to parse ReplyHeader {:?}", e);
                    // self.inflight.pop_front();
                    // return;
                }
            };
            if header.zxid > 0 {
                // Update last-seen zxid when this is a request response
                self.zxid = header.zxid;
            }
            let response = RawResponse {
                header: header,
                data: Cursor::new(data.bytes().to_vec()),
            }; // TODO COPY!
            match response.header.xid {
                // NOTIFICATION_XID
                NOTIFICATION_XID => {
                    info!("handle_response Got a watch event, header: {:?}", response.header);
                    self.watch_sender.send(WatchMessage::Event(response)).unwrap();
                }
                // PING_XID
                PING_XID => {
                    let ping_time_cost = self.ping_sent.elapsed();
                    if ping_time_cost.as_millis() > (self.timeout_ms / 4) as u128 {
                        // too slow between the client the zk server
                        warn!("It is too slow to get ping response, ping cost: {:?}, timeout: {:?}",
                            ping_time_cost, self.timeout_ms);
                    } else {
                        trace!("Got ping response in {:?}", ping_time_cost);
                    }
                    self.inflight.pop_front();
                }
                AUTHPACKET_XID => {
                    error!("Got auth pack xid");
                    panic!("Got auth pack xid");
                }
                SET_WATCHES_XID => {
                    info!("=====Got set watches xid: {:?}", response);
                    self.inflight.pop_front();
                }
                _ => {
                    info!("handle_response Got other event, header: {:?}", response.header);
                    match self.inflight.pop_front() {
                        Some(request) => {
                            if request.opcode == OpCode::CloseSession {
                                warn!("Got the close session request, will close the io event loop");
                                let old_state = self.state;
                                self.state = ZkState::Closed;
                                self.notify_state(old_state, self.state);
                                self.shutdown = true;
                            }
                            self.send_response(request, response);
                        }
                        None => {
                            error!("Shouldn't happen, no inflight request");
                            panic!("Shouldn't happen, no inflight request");
                        }
                    }
                }
            }
        } else {
            self.inflight.pop_front(); // drop the connect request

            let conn_resp = match ConnectResponse::read_from(&mut data) {
                Ok(conn_resp) => conn_resp,
                Err(e) => {
                    error!("Failed to parse ConnectResponse {:?}", e);
                    panic!("Failed to parse ConnectResponse {:?}", e);
                    // self.reconnect();
                    // return
                }
            };

            // TODO clear timeout
            self.clear_timeout(ZkTimeout::Connect);

            let old_state = self.state;
            if conn_resp.timeout <= 0 {
                // we don't get the right timeout from the server, and should close the connection.
                warn!("Unable to connect the session {} expired when get the zero or negative timeout",
                    self.conn_resp.session_id);
                self.conn_resp.session_id = 0;
                self.state = ZkState::Closed;
                self.shutdown = true;
            } else {
                // created the new connection from the server
                self.conn_resp = conn_resp;
                info!("Connected: {:?}", self.conn_resp);
                self.timeout_ms = self.conn_resp.timeout;

                self.conn_timeout_duration = Duration::from_millis(self.timeout_ms / self.hosts.size() as u64);
                self.ping_timeout_duration = Duration::from_millis(min(self.timeout_ms / 3,
                                                                       MAX_INTERVAL_PING_MS));

                info!("Io event create the connection with server successfully, \
                with session timeout {:?}, connect timeout {:?}, ping timeout {:?}",
                    Duration::from_millis(self.timeout_ms), self.conn_timeout_duration, self.ping_timeout_duration);
                if (self.timeout_ms / self.hosts.size() as u64) < MIN_INTERVAL_CONNECT_SERVER_MS {
                    warn!("Io event create the connection, but the connect time {:?} is smaller thant {:?}",
                        self.conn_timeout_duration, Duration::from_millis(MIN_INTERVAL_CONNECT_SERVER_MS));
                }

                self.state = if self.conn_resp.read_only {
                    ZkState::ConnectedReadOnly
                } else {
                    ZkState::Connected
                };
            }

            self.notify_state(old_state, self.state);
        }
    }

    fn update_session_timeout(&mut self) {
        trace!("update session timeout");
        self.start_session_timeout();
        self.session_sent = Instant::now();
    }

    fn send_response(&self, request: RawRequest, response: RawResponse) {
        match request.listener {
            Some(ref listener) => {
                trace!("send_response Opcode is {:?}", request.opcode);
                listener.send(response).unwrap();
            }
            None => info!("Nobody is interested in response {:?}", request.opcode),
        }
        if let Some(watch) = request.watch {
            self.watch_sender.send(WatchMessage::Watch(watch)).unwrap();
        }
    }

    fn clear_session_timeout(&mut self) {
        let timeout = mem::replace(&mut self.session_timeout , None);
        if let Some(timeout) = timeout {
            trace!("clear_session_timeout: Session");
            self.session_timer.cancel_timeout(&timeout);
        }
    }

    fn clear_timeout(&mut self, atype: ZkTimeout) {
        let timeout = match atype {
            ZkTimeout::Ping => {
                mem::replace(&mut self.ping_timeout , None)
            },
            ZkTimeout::Connect => {
                mem::replace(&mut self.conn_timeout , None)
            },
        };
        if let Some(timeout) = timeout {
            trace!("clear_timeout: {:?}", atype);
            self.timer.cancel_timeout(&timeout);
        }
    }

    fn start_session_timeout(&mut self) {
        self.clear_session_timeout();
        trace!("start_session_timeout: Session");
        let duration = Duration::from_millis(self.timeout_ms + 1000);
        self.session_timeout = Some(self.session_timer.set_timeout(duration, ZkSessionTimeout::Session));
        self.poll.reregister(&self.session_timer, SESSION_TIMER, Ready::readable(), pollopt())
            .expect("Reregister SESSION TIMER");
    }


    fn start_timeout(&mut self, atype: ZkTimeout) {
        self.clear_timeout(atype.clone());
        trace!("start_timeout: {:?}", atype);
        match atype {
            ZkTimeout::Ping => {
                let duration = self.ping_timeout_duration.clone();
                self.ping_timeout = Some(self.timer.set_timeout(duration, atype));
            },
            ZkTimeout::Connect => {
                let duration = self.conn_timeout_duration.clone();
                self.conn_timeout = Some(self.timer.set_timeout(duration, atype));
            },
        }
        self.poll.reregister(&self.timer, TIMER, Ready::readable(), pollopt())
            .expect("Reregister TIMER");
    }

    fn is_alive(&self) -> bool {
        self.state != ZkState::Closed && self.state != ZkState::AuthFailed
    }

    fn reconnect_by_close(&mut self) {

        // clear the session timeout
        self.clear_session_timeout();

        // let old_state = self.state;
        // self.state = ZkState::Closed;
        // info!("reconnect by close: from state {:?} to state {:?}", old_state, ZkState::Closed);
        // self.notify_state(old_state, self.state);
        // self.shutdown = true;


        self.reconnect();
    }

    fn reconnect(&mut self) {
        trace!("reconnect");
        if !self.is_alive() {
            // can't create the new connection after switch to the not alive
            panic!("The io event state is not alive, can't reconnect the server");
        }
        let old_state = self.state;
        self.state = ZkState::Connecting;
        self.notify_state(old_state, self.state);

        info!("Establishing Zk connection");

        // TODO only until session times out
        loop {
            self.buffer.clear();
            self.inflight.clear();
            self.response.clear(); // TODO drop all read bytes once RingBuf.clear() is merged

            // Check if the session is still alive according to our knowledge
            // TODO reconnect don't do this
            // if self.ping_sent.elapsed().as_secs() * 1000 > self.timeout_ms {
            //     error!("Because the client doesn't ping server in time, the zk session may timeout, closing io event loop");
            //     self.state = ZkState::Closed;
            //     self.notify_state(ZkState::Connecting, self.state);
            //     self.shutdown = true;
            //     break;
            // }

            self.clear_timeout(ZkTimeout::Ping);
            self.clear_timeout(ZkTimeout::Connect);
            {
                let host = self.hosts.get();
                info!("Connecting to new server {:?}", host);
                self.sock = match TcpStream::connect(host) {
                    Ok(sock) =>{
                        // https://docs.rs/mio/latest/mio/net/struct.TcpStream.html#method.connect
                        // The returned TcpStream may not be connected (and thus usable),
                        // unlike the API found in std::net::TcpStream.
                        // Because Mio issues a non-blocking connect
                        // it will not block the thread and instead return an unconnected TcpStream.
                        sock
                    },
                    Err(e) => {
                        error!("Failed to connect {:?}: {:?}", host, e);
                        continue;
                    }
                };
                info!("Started connecting to {:?}", host);
            }
            self.start_timeout(ZkTimeout::Connect);

            // connect request
            let request = self.connect_request();
            self.buffer.push_back(request);

            // set watches request
            // TODO: get all watches path
            let watches_request = self.watch_request();
            self.buffer.push_back(watches_request);


            // Register the new socket
            let pollopt = PollOpt::edge() | PollOpt::oneshot();
            self.poll.register(&self.sock, ZK, Ready::all(), pollopt)
                .expect("Register ZK");

            break;
        }
    }

    fn connect_request(&self) -> RawRequest {
        let conn_req = ConnectRequest::from(&self.conn_resp, self.zxid);
        let buf = conn_req.to_len_prefixed_buf().unwrap();
        RawRequest {
            opcode: OpCode::Auth,
            data: buf,
            listener: None,
            watch: None,
        }
    }

    fn watch_request(&self) -> RawRequest {
        let record = SetWatchesRequest {
            relateive_zxid: self.zxid,
            data_watches: vec![],
            exist_watches: vec![],
            child_watches: vec![],
        };
        let opcode = OpCode::SetWatches;
        let request_header = RequestHeader {
            xid: SET_WATCHES_XID,
            opcode,
        };
        let buf = to_len_prefixed_buf(request_header, record).expect("can't serialize the set watches request");
        RawRequest {
            opcode,
            data: buf,
            listener: None,
            watch: None,
        }
    }

    fn ready(&mut self, token: Token, ready: Ready) {
        trace!("event token={:?} ready={:?}", token, ready);

        match token {
            ZK => self.ready_zk(ready),
            TIMER => self.ready_timer(ready),
            CHANNEL => self.ready_channel(ready),
            SESSION_TIMER => self.ready_session_timer(ready),
            _ => unreachable!(),
        }
    }

    fn ready_zk(&mut self, ready: Ready) {
        self.clear_timeout(ZkTimeout::Ping);

        if ready.is_writable() {
            while let Some(mut request) = self.buffer.pop_front() {
                match self.sock.try_write_buf(&mut request.data) {
                    Ok(Some(0)) => {
                        warn!("Connection write closed: op {:?}, watch {:?}", request.opcode, request.watch);
                        self.reconnect_by_close();
                        // self.reconnect();
                        return;
                    }
                    Ok(Some(written)) => {
                        if request.opcode == OpCode::Ping {
                            trace!("Written {:?} bytes, op: {:?}", written, request.opcode);
                        } else {
                            info!("Written {:?} bytes, op: {:?}, watch {:?}", written, request.opcode, request.watch);
                        }
                        if request.data.has_remaining() {
                            error!("Data are not been written to the sock, should panic");
                            panic!("Data are not been written to the sock, should panic");
                            // self.buffer.push_front(request);
                            // break;
                        } else {
                            self.inflight.push_back(request);
                        }
                    }
                    Ok(None) => trace!("Spurious write"),
                    Err(e) => {
                        // log the write op and data
                        error!("Failed to write socket: {:?}, write data op {:?}, watch {:?}",
                                    e, request.opcode, request.watch);
                        match e.kind() {
                            ErrorKind::WouldBlock => error!("Got WouldBlock IO Error, no need to reconnect."),
                            _ => {
                                self.reconnect_by_close();
                                // self.reconnect();
                                return;
                            }
                        }
                    }
                }
            }
        }

        if ready.is_readable() {
            match self.sock.try_read_buf(&mut self.response) {
                Ok(Some(0)) => {
                    warn!("Connection read closed");
                    self.reconnect_by_close();
                    // self.reconnect();
                    return;
                }
                Ok(Some(read)) => {
                    trace!("Read {:?} bytes", read);
                    self.handle_response();
                    // when receive response from the zk server,
                    self.update_session_timeout();
                }
                Ok(None) => trace!("Spurious read"),
                Err(e) => {
                    error!("Failed to read socket: {:?}", e);
                    match e.kind() {
                        ErrorKind::WouldBlock => trace!("Got WouldBlock IO Error, no need to reconnect."),
                        _ => {
                            self.reconnect_by_close();
                            // self.reconnect();
                            return;
                        }
                    }
                }
            }
        }

        if (ready.is_hup()) && (self.state != ZkState::Closed) {
            // If we were connected
            // fn send_watched_event(keeper_state: KeeperState) {
            //     match sender.send(WatchedEvent{event_type: WatchedEventType::None,
            //                                    keeper_state: keeper_state,
            //                                    path: None}) {
            //         Ok(()) => (),
            //         Err(e) => panic!("Reader/Writer: Event died {}", e)
            //     }
            // }

            error!("Want to set the io event state to {:?}, but set it to close and panic", ZkState::NotConnected);
            let old_state = self.state;
            self.state = ZkState::Closed;
            self.shutdown = true;
            self.notify_state(old_state, self.state);
            info!("Reconnect due to HUP");
            // self.reconnect();
        }

        self.start_timeout(ZkTimeout::Ping);

        // if self.is_idle() {
        //     self.start_timeout(ZkTimeout::Ping);
        // }

        // Not sure that we need to write, but we always need to read, because of watches
        // If the output buffer has no content, we don't need to write again
        let mut interest = Ready::all();
        if self.buffer.is_empty() {
            interest.remove(Ready::writable());
        }

        // This tick is done, subscribe to a forthcoming one
        self.reregister(interest);
    }

    fn is_idle(&self) -> bool {
        self.inflight.is_empty() && self.buffer.is_empty()
    }

    fn ready_channel(&mut self, _: Ready) {
        while let Ok(request) = self.rx.try_recv() {
            trace!("ready_channel {:?}", request.opcode);

            match self.state {
                ZkState::Closed => {
                    // If zk is unavailable, respond with a ConnectionLoss error.
                    let header = ReplyHeader {
                        xid: 0,
                        zxid: 0,
                        err: ZkError::ConnectionLoss as i32,
                    };
                    let response = RawResponse {
                        header: header,
                        data: ByteBuf::new(vec![]),
                    };
                    self.send_response(request, response);
                },
                _ => {
                    // Otherwise, queue request for processing.
                    if self.buffer.is_empty() {
                        self.reregister(Ready::all());
                    }
                    self.buffer.push_back(request);
                },
            }
        }

        self.poll.reregister(&self.rx, CHANNEL, Ready::readable(), pollopt())
            .expect("Reregister CHANNEL");
    }

    fn ready_session_timer(&mut self, _: Ready) {
        trace!("ready_session_timer thread={:?}", ::std::thread::current().id());
        loop {
            match self.session_timer.poll() {
                Some(ZkSessionTimeout::Session) => {
                    warn!("handle session timeout: client session timeout, have not heard from server in {:?}",
                        self.session_sent.elapsed());

                    // TODO can be deleted.
                    self.clear_session_timeout();
                    let old_state = self.state;
                    self.state = ZkState::Closed;
                    info!("reconnect by close: from state {:?} to state {:?}", old_state, ZkState::Closed);
                    self.notify_state(old_state, self.state);
                    self.shutdown = true;

                    // self.reconnect_by_close();
                },
                None => {
                    if self.session_timeout.is_some() {
                        trace!("Spurious session timer");
                        self.poll.reregister(&self.session_timer, SESSION_TIMER, Ready::readable(), pollopt())
                            .expect("Reregister SESSION TIMER");
                    }
                    break;
                }
            }
        }
    }

    fn ready_timer(&mut self, _: Ready) {
        trace!("ready_timer thread={:?}", ::std::thread::current().id());

        loop {
            match self.timer.poll() {
                Some(ZkTimeout::Ping) => {
                    trace!("handle ping timeout");
                    self.clear_timeout(ZkTimeout::Ping);
                    if self.inflight.is_empty() {
                        // No inflight request indicates an idle connection. Send a ping.
                        trace!("Pinging {:?}", self.sock.peer_addr().unwrap());
                        self.tx.send(RawRequest {
                            opcode: OpCode::Ping,
                            data: PING.clone(),
                            listener: None,
                            watch: None,
                        }).unwrap();
                        self.ping_sent = Instant::now();
                    }
                },
                Some(ZkTimeout::Connect) => {
                    info!("handle connection timeout");
                    self.clear_timeout(ZkTimeout::Connect);
                    if self.state == ZkState::Connecting {
                        info!("Reconnect due to connection timeout");
                        self.reconnect_by_close();
                        // self.reconnect();
                    }
                },
                None => {
                    if self.ping_timeout.is_some() || self.conn_timeout.is_some() {
                        trace!("Spurious timer");
                        self.poll.reregister(&self.timer, TIMER, Ready::readable(), pollopt())
                            .expect("Reregister TIMER");
                    }
                    break;
                }
            }
        }
    }

    pub fn sender(&self) -> Sender<RawRequest> {
        self.tx.clone()
    }

    pub fn run(mut self) -> io::Result<()> {
        let mut events = Events::with_capacity(128);

        // Register Initial Interest
        self.poll.register(&self.sock, ZK, Ready::all(), pollopt())
            .expect("Register ZK");
        self.poll.register(&self.timer, TIMER, Ready::readable(), pollopt())
            .expect("Register TIMER");
        self.poll.register(&self.rx, CHANNEL, Ready::readable(), pollopt())
            .expect("Register CHANNEL");
        // session timer
        self.poll.register(&self.session_timer, SESSION_TIMER, Ready::readable(), pollopt())
            .expect("Register SESSION_TIMER");

        // start the session timeout
        self.update_session_timeout();

        loop {
            // Handle loop shutdown
            if self.shutdown {
                info!("The zk io event loop was shutdown");
                break;
            }

            // Wait for events
            self.poll.poll(&mut events, None)?;

            // Process events
            for event in &events {
                self.ready(event.token(), event.readiness());
            }
        }
        info!("End of the zk io event loop");
        Ok(())
    }
}
