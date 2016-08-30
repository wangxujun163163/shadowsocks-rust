use std::fmt;
use std::str;
use std::rc::Rc;
use std::cell::RefCell;
use std::io::Cursor;
use std::net::SocketAddr;

use rand;
use regex::Regex;
use mio::{Token, EventSet, EventLoop, PollOpt};
use mio::udp::UdpSocket;

use relay::{Relay, Processor};
use util::{handle_every_line, Dict, slice2str, slice2string};
use network::{is_ip, slice2ip4, slice2ip6, str2addr4, NetworkWriteBytes, NetworkReadBytes};

// All communications inside of the domain protocol are carried in a single
// format called a message.  The top level format of message is divided
// into 5 sections (some of which are empty in certain cases) shown below:
//
//     +---------------------+
//     |        Header       |
//     +---------------------+
//     |       Question      | the question for the name server
//     +---------------------+
//     |        Answer       | RRs answering the question
//     +---------------------+
//     |      Authority      | RRs pointing toward an authority
//     +---------------------+
//     |      Additional     | RRs holding additional information
//     +---------------------+
//
// The header section is always present.  The header includes fields that
// specify which of the remaining sections are present, and also specify
// whether the message is a query or a response, a standard query or some
// other opcode, etc.

// The header section format:
//
//                                     1  1  1  1  1  1
//       0  1  2  3  4  5  6  7  8  9  0  1  2  3  4  5
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |                      ID                       |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |QR|   Opcode  |AA|TC|RD|RA|   Z    |   RCODE   |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |                    QDCOUNT                    |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |                    ANCOUNT                    |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |                    NSCOUNT                    |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |                    ARCOUNT                    |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+

const QTYPE_ANY  : u16 = 255;
const QTYPE_A    : u16 = 1;
const QTYPE_AAAA : u16 = 28;
const QTYPE_CNAME: u16 = 5;
const QTYPE_NS   : u16 = 2;
const QCLASS_IN  : u16 = 1;

type ResponseRecord = (String, String, u16, u16);
type ResponseHeader = (u16, u16, u16, u16, u16, u16, u16, u16, u16);


pub trait Caller {
    fn handle_dns_resolved(&mut self, event_loop: &mut EventLoop<Relay>, Option<(String, String)>, Option<&str>);
}

// For detail, see page 7 of RFC 1035
fn build_address(address: &str) -> Option<Vec<u8>> {
    let mut v = vec![];
    let bytes = address.as_bytes();
    for label in bytes.split(|b| *b == '.' as u8) {
        match label.len() {
            0 => {
                continue;
            }
            n if n > 63 => {
                return None;
            }
            n => {
                v.push(n as u8);
                v.extend(label);
            }
        }
    }

    v.push(0);
    Some(v)
}

// For detail, see page 24 of RFC 1035
fn build_request(address: &str, qtype: u16) -> Option<Vec<u8>> {
    let mut r = vec![];
    // The header section:
    //
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |               random request_id               |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     | 0|     0     | 0| 0| 1| 0|   0    |     0     |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                       1                       |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                       0                       |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                       0                       |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                       0                       |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    let request_id = rand::random::<u16>();
    try_opt!(r.put_u16(request_id));
    try_opt!(r.put_u8(1));
    try_opt!(r.put_u8(0));
    try_opt!(r.put_u16(1));
    try_opt!(r.put_u16(0));
    try_opt!(r.put_u16(0));
    try_opt!(r.put_u16(0));
    // address
    match build_address(address) {
        Some(addr) => r.extend(addr),
        None => {
            return None;
        }
    }
    // qtype and qclass
    try_opt!(r.put_u16(qtype));
    try_opt!(r.put_u16(QCLASS_IN));

    Some(r)
}


// RDATA: a variable length string of octets that describes the resource.
//        The format of this information varies according to the TYPE and CLASS
//        of the resource record. For example, the if the TYPE is A
//        and the CLASS is IN, the RDATA field is a 4 octet ARPA Internet address.
fn parse_ip(addrtype: u16, data: &[u8], length: usize, offset: usize) -> Option<String> {
    let ip_part = &data[offset..offset + length];

    let ip = match addrtype {
        QTYPE_A => slice2ip4(ip_part),
        QTYPE_AAAA => slice2ip6(ip_part),
        QTYPE_CNAME | QTYPE_NS => try_opt!(parse_name(data, offset as u16)).1,
        _ => String::from(try_opt!(slice2str(ip_part))),
    };

    Some(ip)
}

// For detail, see page 29 of RFC 1035
fn parse_name(data: &[u8], offset: u16) -> Option<(u16, String)> {
    let mut p = offset as usize;
    let mut l = data[p];
    let mut labels: Vec<String> = Vec::new();

    while l > 0 {
        // if compressed
        if (l & 0b11000000) == 0b11000000 {
            //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
            //    | 1  1|                OFFSET                   |
            //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
            let mut ptr = try_opt!(Cursor::new(&data[p..p + 2]).get_u16());
            ptr &= 0x3FFF;
            let r = try_opt!(parse_name(data, ptr));
            labels.push(r.1);
            p += 2;
            return Some((p as u16 - offset, labels.join(".")));
        } else {
            labels.push(try_opt!(slice2string(&data[(p + 1)..(p + 1 + l as usize)])));
            p += 1 + l as usize;
        }

        l = data[p];
    }

    Some((p as u16 + 1 - offset, labels.join(".")))
}

// For detail, see page 27, 28 of RFC 1035
fn parse_record(data: &[u8], offset: u16, question: bool) -> Option<(u16, ResponseRecord)> {
    let (nlen, name) = try_opt!(parse_name(data, offset));

    // The question section format:
    //
    //                                     1  1  1  1  1  1
    //       0  1  2  3  4  5  6  7  8  9  0  1  2  3  4  5
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                                               |
    //     /                     QNAME                     /
    //     /                                               /
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                     QTYPE                     |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                     QCLASS                    |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    let res = if question {
        let bytes = &data[(offset + nlen) as usize ..(offset + nlen + 4) as usize];
        let mut record = Cursor::new(bytes);

        let record_type = try_opt!(record.get_u16());
        let record_class = try_opt!(record.get_u16());

        (nlen + 4, (name, String::new(), record_type, record_class))
    //                                    1  1  1  1  1  1
    //      0  1  2  3  4  5  6  7  8  9  0  1  2  3  4  5
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //    |                                               |
    //    /                                               /
    //    /                      NAME                     /
    //    |                                               |
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //    |                      TYPE                     |
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //    |                     CLASS                     |
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //    |                      TTL                      |
    //    |                                               |
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //    |                   RDLENGTH                    |
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--|
    //    /                     RDATA                     /
    //    /                                               /
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    } else {
        let bytes = &data[(offset + nlen) as usize ..(offset + nlen + 10) as usize];
        let mut record = Cursor::new(bytes);

        let record_type = try_opt!(record.get_u16());
        let record_class = try_opt!(record.get_u16());
        let _record_ttl = try_opt!(record.get_u32());
        let record_rdlength = try_opt!(record.get_u16());

        // RDATA
        let ip = try_opt!(parse_ip(record_type,
                                   data,
                                   record_rdlength as usize,
                                   (offset + nlen + 10) as usize));

        (nlen + 10 + record_rdlength, (name, ip, record_type, record_class))
    };

    Some(res)
}

fn parse_header(data: &[u8]) -> Option<ResponseHeader> {
    if data.len() < 12 {
        return None;
    }

    let mut header = Cursor::new(data);

    let id      = try_opt!(header.get_u16());
    let byte3   = try_opt!(header.get_u8());
    let byte4   = try_opt!(header.get_u8());
    let qdcount = try_opt!(header.get_u16());
    let ancount = try_opt!(header.get_u16());
    let nscount = try_opt!(header.get_u16());
    let arcount = try_opt!(header.get_u16());
    let qr      = (byte3 & 0b10000000) as u16;
    let tc      = (byte3 & 0b00000010) as u16;
    let ra      = (byte4 & 0b00000010) as u16;
    let rcode   = (byte4 & 0b00001111) as u16;

    Some((id, qr, tc, ra, rcode, qdcount, ancount, nscount, arcount))
}

fn parse_records(data: &[u8], offset: u16, count: u16, question: bool) -> Option<(u16, Vec<ResponseRecord>)> {
    let mut records: Vec<ResponseRecord> = Vec::new();
    let mut offset = offset;

    for _i in 0..count {
        let (len, record) = try_opt!(parse_record(data, offset, question));
        offset += len;
        records.push(record);
    }

    Some((offset, records))
}

fn parse_response(data: &[u8]) -> Option<DNSResponse> {
    if data.len() < 12 {
        return None;
    }

    match parse_header(data) {
        Some(header) => {
            let (_id, _qr, _tc, _ra, _rcode, qdcount, ancount, _nscount, _arcount) = header;

            let offset = 12u16;
            let (offset, qds) = try_opt!(parse_records(data, offset, qdcount, true));
            let (_offset, ans) = try_opt!(parse_records(data, offset, ancount, false));
            // We don't need to parse the authority records and the additional records
            let (_offset, _nss) = try_opt!(parse_records(data, _offset, _nscount, false));
            let (_offset, _ars) = try_opt!(parse_records(data, _offset, _arcount, false));

            let mut response = DNSResponse::new();
            if qds.len() > 0 {
                response.hostname = qds[0].0.clone();
            }
            for an in qds {
                response.questions.push((an.1, an.2, an.3))
            }
            for an in ans {
                response.answers.push((an.1, an.2, an.3))
            }

            Some(response)
        }
        None => None
    }
}

// For detail, see page 7 of RFC 1035
fn is_valid_hostname(hostname: &str) -> bool {
    if hostname.len() > 255 {
        return false;
    }

    lazy_static! {
        static ref RE: Regex = Regex::new(r"[A-Za-z\d-]{1,63}$").unwrap();
    }

    let hostname = hostname.trim_right_matches('.');
    hostname.as_bytes()
        .split(|c| *c == b'.')
        .all(|s| {
            let s = slice2str(s).unwrap_or("");
            s.len() > 0 && !s.starts_with("-") && !s.ends_with("-") && RE.is_match(s)
        })
}

struct DNSResponse {
    hostname: String,
    questions: Vec<(String, u16, u16)>,
    answers: Vec<(String, u16, u16)>
}

impl DNSResponse {
    fn new() -> DNSResponse {
        DNSResponse {
            hostname: String::new(),
            questions: Vec::new(),
            answers: Vec::new()
        }
    }
}

impl fmt::Debug for DNSResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {:?}", self.hostname, self.answers)
    }
}


#[derive(Clone, Copy)]
enum HostnameStatus {
    First,
    Second,
}

pub type Callback = FnMut(&mut Caller, Option<(String, String)>, Option<&str>);

pub struct DNSResolver {
    token: Option<Token>,
    hosts: Dict<String, String>,
    cache: Dict<String, String>,
    callers: Dict<Token, Rc<RefCell<Caller>>>,
    hostname_status: Dict<String, HostnameStatus>,
    hostname_to_caller: Dict<String, Vec<Rc<RefCell<Caller>>>>,
    sock: Option<UdpSocket>,
    servers: Vec<String>,
    qtypes: Vec<u16>,
}

// TODO: add LRU `self.cache` to cache query result, see https://github.com/contain-rs/lru-cache
impl DNSResolver {
    pub fn new(server_list: Option<Vec<String>>, prefer_ipv6: Option<bool>) -> DNSResolver {
        let mut this = DNSResolver {
            token: None,
            servers: Vec::new(),
            hosts: Dict::new(),
            cache: Dict::new(),
            callers: Dict::new(),
            hostname_status: Dict::new(),
            hostname_to_caller: Dict::new(),
            sock: UdpSocket::v4().ok(),
            qtypes: Vec::new(),
        };

        match server_list {
            Some(servers) => this.servers = servers,
            None => this.parse_resolv(),
        }

        if prefer_ipv6.is_some() && prefer_ipv6.unwrap() {
            this.qtypes = vec![QTYPE_AAAA, QTYPE_A];
        } else {
            this.qtypes = vec![QTYPE_A, QTYPE_AAAA];
        }
        this.parse_hosts();

        this
    }

    pub fn add_caller(&mut self, token: Token, caller: Rc<RefCell<Caller>>) {
        self.callers.put(token, caller);
    }

    pub fn remove_caller(&mut self, token: Token) -> Option<Rc<RefCell<Caller>>> {
        self.callers.del(&token)
    }

    fn parse_resolv(&mut self) {
        handle_every_line("/etc/resolv.conf", &mut |line| {
            if line.starts_with("nameserver") {
                if let Some(server) = line.split_whitespace().nth(1) {
                    if is_ip(server) {
                        self.servers.push(server.to_string());
                    }
                }
            }
        });

        if self.servers.len() == 0 {
            self.servers = vec!["8.8.4.4", "8.8.8.8"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
    }

    fn parse_hosts(&mut self) {
        handle_every_line("/etc/hosts", &mut |line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() > 0 {
                let ip = parts[0];
                if is_ip(ip) {
                    for hostname in parts[1..].iter() {
                        if hostname.len() > 0 {
                            self.hosts.put(hostname.to_string(), ip.to_string());
                        }
                    }
                }
            }
        });

        self.hosts.put("localhost".to_string(), "127.0.0.1".to_string());
    }

    fn send_request(&self, hostname: String, qtype: u16) {
        let req = build_request(&hostname, qtype).unwrap();
        match self.sock {
            Some(ref sock) => {
                trace!("send query request of {} to servers", &hostname);
                for server in self.servers.iter() {
                    let server = format!("{}:53", server);
                    let addr = SocketAddr::V4(str2addr4(&server).unwrap());
                    if let Err(e) = sock.send_to(&req, &addr) {
                        error!("{}", e);
                        return;
                    }
                }
            }
            None => error!("DNS socket closed"),
        }
    }

    pub fn resolve(&mut self, event_loop: &mut EventLoop<Relay>, hostname: String, caller_token: Token) {
        if let Some(caller) = self.callers.get(&caller_token) {
            if hostname.len() == 0 {
                unsafe {
                    let caller = &mut *(caller).as_ptr();
                    caller.handle_dns_resolved(event_loop, None, Some("empty hostname"));
                }
            } else if is_ip(&hostname) {
                unsafe {
                    let caller = &mut *(caller).as_ptr();
                    caller.handle_dns_resolved(event_loop, Some((hostname.clone(), hostname)), None);
                }
            } else if self.hosts.has(&hostname) {
                let ip = self.hosts.get(&hostname).unwrap().clone();
                unsafe {
                    let caller = &mut *(caller).as_ptr();
                    caller.handle_dns_resolved(event_loop, Some((hostname, ip)), None);
                }
            } else if self.cache.has(&hostname) {
                let ip = self.cache.get(&hostname).unwrap().clone();
                unsafe {
                    let caller = &mut *(caller).as_ptr();
                    caller.handle_dns_resolved(event_loop, Some((hostname, ip)), None);
                }
            } else if !is_valid_hostname(&hostname) {
                let errmsg = format!("invalid hostname: {}", hostname);
                unsafe {
                    let caller = &mut *(caller).as_ptr();
                    caller.handle_dns_resolved(event_loop, None, Some(&errmsg));
                }
            } else {
                if self.hostname_to_caller.has(&hostname) {
                    let arr = self.hostname_to_caller.get_mut(&hostname).unwrap();
                    arr.push(caller.clone());
                } else {
                    self.hostname_status.put(hostname.clone(), HostnameStatus::First);
                    self.hostname_to_caller.put(hostname.clone(), vec![caller.clone()]);
                }

                self.send_request(hostname, self.qtypes[0]);
            }
        } else {
            info!("caller {:?} does not exists", caller_token);
        }
    }

    fn call_callback(&mut self, event_loop: &mut EventLoop<Relay>, hostname: String, ip: String) {
        if let Some(callers) = self.hostname_to_caller.get_mut(&hostname) {
            for caller in callers.iter_mut() {
                let errmsg = format!("unknown hostname {}", hostname.clone());

                let error = if ip.len() > 0 {
                    None
                } else {
                    Some(errmsg.as_str())
                };

                unsafe {
                    let caller = &mut *(caller).as_ptr();
                    caller.handle_dns_resolved(event_loop, Some((hostname.clone(), ip.clone())), error);
                }
            }
        }

        if self.hostname_to_caller.has(&hostname) {
            self.hostname_to_caller.del(&hostname);
        }
        if self.hostname_status.has(&hostname) {
            self.hostname_status.del(&hostname);
        }
    }

    fn handle_data(&mut self, event_loop: &mut EventLoop<Relay>, data: &[u8]) {
        match parse_response(data) {
            Some(response) => {
                let mut ip = String::new();
                for answer in response.answers.iter() {
                    if (answer.1 == QTYPE_A || answer.1 == QTYPE_AAAA) && answer.2 == QCLASS_IN {
                        ip = answer.0.clone();
                        break;
                    }
                }

                let hostname = response.hostname;
                let hostname_status = match self.hostname_status.get(&hostname) {
                    Some(&HostnameStatus::First) => 1,
                    Some(&HostnameStatus::Second) => 2,
                    _ => 0
                };

                if ip.len() == 0 && hostname_status == 1 {
                    self.hostname_status[hostname.clone()] = HostnameStatus::Second;
                    self.send_request(hostname, self.qtypes[1]);
                } else if ip.len() > 0 {
                    self.call_callback(event_loop, hostname, ip);
                } else if hostname_status == 2 {
                    for question in response.questions {
                        if question.1 == self.qtypes[1] {
                            self.call_callback(event_loop, hostname, String::new());
                            break;
                        }
                    }
                }
            }
            None => info!("invalid DNS response"),
        }
    }

    pub fn add_to_loop(&mut self, token: Token, event_loop: &mut EventLoop<Relay>, events: EventSet) {
        if self.sock.is_none() {
            self.sock = UdpSocket::v4().ok();
        }
        self.token = Some(token);

        if let Some(ref socket) = self.sock {
            if event_loop.register(socket, token, events, PollOpt::level()).is_err() {
                error!("add DNSResolver to event_loop failed.");
            }
        } else {
            error!("create UDP socket for DNSResolver failed.");
        }
    }
}

impl Processor for DNSResolver {
    fn process(&mut self, event_loop: &mut EventLoop<Relay>, _token: Token, events: EventSet) {
        if events.is_error() {
            error!("events error happened on DNS socket");
            let sock = self.sock.take();
            if sock.is_some() {
                let sock = sock.unwrap();
                event_loop.deregister(&sock).ok();
            }

            let token = self.token.unwrap();
            self.add_to_loop(token, event_loop, EventSet::readable());
        } else {
            let mut buf = [0u8; 1024];
            let mut recevied = None;

            match self.sock {
                Some(ref sock) => {
                    match sock.recv_from(&mut buf) {
                        Ok(Some((len, _addr))) => {
                            recevied = Some(&buf[..len]);
                        }
                        _ => warn!("receive error on DNS socket"),
                    }
                }
                None => error!("DNS socket closed"),
            }

            if recevied.is_some() {
                self.handle_data(event_loop, recevied.unwrap());
            }
        }
    }

    fn destroy(&mut self, event_loop: &mut EventLoop<Relay>) {
        unimplemented!();
    }

    fn is_destroyed(&self) -> bool {
        self.token == None
    }
}

#[test]
fn test() {
    extern crate env_logger;
    env_logger::init().unwrap();

    // answer of "baidu.com"
    let data: &[u8] = &[
        0x0d, 0x0d, 0x81, 0x80, 0x00, 0x01, 0x00, 0x04,
        0x00, 0x05, 0x00, 0x00, 0x05, 0x62, 0x61, 0x69,
        0x64, 0x75, 0x03, 0x63, 0x6f, 0x6d, 0x00, 0x00,
        0x01, 0x00, 0x01, 0xc0, 0x0c, 0x00, 0x01, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x36, 0x00, 0x04, 0xb4,
        0x95, 0x84, 0x2f, 0xc0, 0x0c, 0x00, 0x01, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x36, 0x00, 0x04, 0xdc,
        0xb5, 0x39, 0xd9, 0xc0, 0x0c, 0x00, 0x01, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x36, 0x00, 0x04, 0x6f,
        0x0d, 0x65, 0xd0, 0xc0, 0x0c, 0x00, 0x01, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x36, 0x00, 0x04, 0x7b,
        0x7d, 0x72, 0x90, 0xc0, 0x0c, 0x00, 0x02, 0x00,
        0x01, 0x00, 0x01, 0x4f, 0x30, 0x00, 0x06, 0x03,
        0x64, 0x6e, 0x73, 0xc0, 0x0c, 0xc0, 0x0c, 0x00,
        0x02, 0x00, 0x01, 0x00, 0x01, 0x4f, 0x30, 0x00,
        0x06, 0x03, 0x6e, 0x73, 0x37, 0xc0, 0x0c, 0xc0,
        0x0c, 0x00, 0x02, 0x00, 0x01, 0x00, 0x01, 0x4f,
        0x30, 0x00, 0x06, 0x03, 0x6e, 0x73, 0x33, 0xc0,
        0x0c, 0xc0, 0x0c, 0x00, 0x02, 0x00, 0x01, 0x00,
        0x01, 0x4f, 0x30, 0x00, 0x06, 0x03, 0x6e, 0x73,
        0x34, 0xc0, 0x0c, 0xc0, 0x0c, 0x00, 0x02, 0x00,
        0x01, 0x00, 0x01, 0x4f, 0x30, 0x00, 0x06, 0x03,
        0x6e, 0x73, 0x32, 0xc0, 0x0c,
    ];

    assert!(parse_response(data).is_some());
}
