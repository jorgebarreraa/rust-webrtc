#![allow(dead_code)]

use glib::{MainContext, BoolError};
use webrtc_sdp::media_type::{SdpMediaValue, SdpMedia};
use webrtc_sdp::attribute_type::{SdpAttributeType, SdpAttribute, SdpAttributeGroup, SdpAttributeGroupSemantic};
use std::fmt::Debug;
use futures::{FutureExt, StreamExt};
use libnice::ice::{Candidate};
use futures::task::{Context, Poll};
use tokio::macros::support::Pin;
use std::rc::Rc;
use std::cell::{RefCell, RefMut};
use std::collections::{BTreeMap, VecDeque};
use crate::transport::{RTCTransport, RTCTransportInitializeError, ICECredentials, RTCTransportEvent, RTCTransportICECandidateAddError, RTCTransportState, RTPTransportSetup, RTCTransportDescriptionApplyError};
use webrtc_sdp::{SdpSession, SdpOrigin, SdpTiming};
use webrtc_sdp::address::ExplicitlyTypedAddress;
use std::net::{IpAddr, Ipv4Addr};
use libnice::ffi::{NiceCompatibility, NiceAgentProperty};
use libnice::sys::{NiceAgentOption_NICE_AGENT_OPTION_ICE_TRICKLE, NiceAgentOption};
use crate::application::{ChannelApplication, DataChannel, ApplicationChannelEvent};
use crate::media::{MediaLine, MediaLineParseError, ActiveInternalMediaReceiver, MediaReceiver, InternalMediaSender, InternalMediaTrack, MediaSender, NegotiationState, InternalMediaReceiver, MediaSenderEvent};
use crate::utils::rtp::ParsedRtpPacket;
use crate::utils::rtcp::RtcpPacket;
use std::ops::{DerefMut};
use crate::sctp::message::DataChannelType;
use std::task::Waker;
use std::io::Error;
use rtp_rs::RtpReaderError;
use slog::{
    slog_trace,
    slog_debug,
    slog_info,
    slog_warn,
    o
};

/// The default setup type if the remote peer offers active and passive setup
/// Allowed values are only `RTPTransportSetup::Passive` and `RTPTransportSetup::Active`
pub const ACT_PASS_DEFAULT_SETUP_TYPE: RTPTransportSetup = RTPTransportSetup::Passive;

pub enum PeerConnectionEvent {
    PeerReset,

    NegotiationNeeded,
    LocalIceCandidate(Option<Candidate>, usize),

    ReceivedRemoteStream(MediaReceiver),
    ReceivedDataChannel(DataChannel),

    /// This event will only be fired if dispatch_unassignable_packets has been enabled
    UnassignableRtcpPacket(RtcpPacket),
    /// This event will only be fired if dispatch_unassignable_packets has been enabled
    UnassignableRtpPacket(ParsedRtpPacket),
    /// This event will only be fired if dispatch_unassignable_packets has been enabled.
    /// If this event gets received it means that we've received application channel data
    /// without having an application channel initialized.
    /// This most likely indicates that something went horrible wong when exchanging the sdp.
    UnassignableDtlsPacket(Vec<u8>),

    /// This event will only be fired if dispatch_undecodable_packets has been enabled.
    /// Attention: The buffer may contains multiple RTCP packets chained to each other.
    /// If so we failed to split them up.
    UndecodableRtcpPacket(Vec<u8>, Error),
    /// This event will only be fired if dispatch_undecodable_packets has been enabled
    UndecodableRtpPacket(Vec<u8>, RtpReaderError)
}

#[derive(Debug, PartialOrd, PartialEq, Clone)]
pub enum RtcDescriptionType {
    Offer,
    Answer
}

#[derive(Debug, PartialOrd, PartialEq, Clone)]
pub enum PeerConnectionState {
    New,
    Connecting,
    Connected,
    Disconnecting,
    Disconnected,
    Failed
}

#[derive(Debug, PartialEq)]
pub enum SignallingState {
    /// Nothing has been negotiated
    None,
    /// A local offer has been created, awaiting the remote answer.
    /// When being in this state, no modifications to the streams are allowed.
    HaveLocalOffer,
    /// Received a remote offer, awaiting the own answer to be generated
    HaveRemoteOffer,
    /// Everything has been negotiated
    Negotiated,
    /// Everything has been negotiated, but something changed
    NegotiationRequired
}

pub struct PeerConnection {
    state: PeerConnectionState,
    signalling_state: SignallingState,

    logger: slog::Logger,

    peer_poll_waker: Option<Waker>,

    ice_agent: libnice::ice::Agent,
    ice_port_range: Option<(u16, u16)>,

    transport: BTreeMap<u32, Rc<RefCell<RTCTransport>>>,
    media_lines: Vec<Rc<RefCell<MediaLine>>>,

    stream_receiver: BTreeMap<u32, Box<dyn InternalMediaReceiver>>,
    stream_sender: BTreeMap<u32, RefCell<InternalMediaSender>>,

    allow_application_channel: bool,
    application_channel: Option<Box<ChannelApplication>>,

    origin_username: String,
    event_queue: VecDeque<PeerConnectionEvent>,

    dispatch_unassignable_packets: bool,
    dispatch_unknown_packets: bool,
    dispatch_undecodable_packets: bool
}

#[derive(Debug)]
pub enum RemoteDescriptionApplyError {
    InvalidNegotiationState,

    UnsupportedMode,
    UnsupportedMediaType{ media_index: usize },
    InvalidSdp{ reason: String },
    InternalError{ detail: String },
    ApplicationChannelHasBeenDisabled,
    ApplicationChannelMissing,

    /// The remote description contains less media lines than
    /// we're expecting
    MissingMediaLines,

    DuplicatedApplicationChannel,
    MissingAttribute{ media_index: usize, attribute: String },
    FailedToAddIceStream{ error: BoolError },

    MediaLineParseError{ media_line: usize, error: MediaLineParseError },

    MediaChannelConfigure{ error: String },
    IceInitializeError{ result: RTCTransportInitializeError, media_line: usize },
    MixedIceSetupStates{ },
    IceSetupUnsupported{ media_index: usize },
    IceConfigureError{ media_line: usize, error: RTCTransportDescriptionApplyError }
}

#[derive(Debug)]
pub enum CreateAnswerError {
    DescribeError(usize),

    MissingTransportChannel(usize),
    InternalError(String),

    InvalidNegotiationState
}

macro_rules! get_attribute_value {
    ($media:expr, $index:ident, $type:ident) => {
        $media.get_attribute(SdpAttributeType::$type)
            .and_then(|attr| if let SdpAttribute::$type(value) = attr { Some(value) } else { None })
            .ok_or(RemoteDescriptionApplyError::MissingAttribute { media_index: $index, attribute: SdpAttributeType::$type.to_string() })
    }
}

pub struct PeerConnectionBuilder {
    event_loop: Option<MainContext>,
    logger: Option<slog::Logger>,

    stun: Option<(String, u16)>,

    nice_compatibility: NiceCompatibility,
    nice_flags: NiceAgentOption,

    allow_application_channel: bool,

    ice_port_range: Option<(u16, u16)>,

    ice_tcp: bool,
    ice_udp: bool,

    ice_upnp: bool,

    dispatch_unassignable_packets: bool,
    dispatch_unknown_packets: bool,
    dispatch_undecodable_packets: bool,
}

impl PeerConnectionBuilder {
    fn new() -> Self {
        PeerConnectionBuilder {
            event_loop: None,
            logger: None,

            stun: None,

            nice_compatibility: NiceCompatibility::RFC5245,
            nice_flags: NiceAgentOption_NICE_AGENT_OPTION_ICE_TRICKLE,

            allow_application_channel: true,

            ice_port_range: None,

            ice_tcp: true,
            ice_udp: true,

            ice_upnp: false,

            dispatch_unassignable_packets: false,
            dispatch_undecodable_packets: false,
            dispatch_unknown_packets: false
        }
    }

    pub fn event_loop(&mut self, context: MainContext) -> &mut Self {
        self.event_loop = Some(context);
        self
    }

    pub fn logger(&mut self, logger: slog::Logger) -> &mut Self {
        self.logger = Some(logger);
        self
    }

    pub fn stun(&mut self, server: (String, u16)) -> &mut Self {
        self.stun = Some(server);
        self
    }

    pub fn nice_compatibility(&mut self, compatibility: NiceCompatibility) -> &mut Self {
        self.nice_compatibility = compatibility;
        self
    }

    pub fn nice_flags(&mut self, flags: NiceAgentOption) -> &mut Self {
        self.nice_flags = flags;
        self
    }

    pub fn allow_application_channel(&mut self, enabled: bool) -> &mut Self {
        self.allow_application_channel = enabled;
        self
    }

    pub fn ice_port_range(&mut self, port_range: (u16, u16)) -> &mut Self {
        self.ice_port_range = Some(port_range);
        self
    }

    pub fn ice_tcp(&mut self, enabled: bool) -> &mut Self {
        self.ice_tcp = enabled;
        self
    }

    pub fn ice_udp(&mut self, enabled: bool) -> &mut Self {
        self.ice_udp = enabled;
        self
    }

    pub fn ice_upnp(&mut self, enabled: bool) -> &mut Self {
        self.ice_upnp = enabled;
        self
    }

    pub fn dispatch_unassignable_packets(&mut self, enabled: bool) -> &mut Self {
        self.dispatch_unassignable_packets = enabled;
        self
    }

    pub fn dispatch_undecodable_packets(&mut self, enabled: bool) -> &mut Self {
        self.dispatch_unassignable_packets = enabled;
        self
    }

    pub fn dispatch_unknown_packets(&mut self, enabled: bool) -> &mut Self {
        self.dispatch_unassignable_packets = enabled;
        self
    }

    /// Alias for
    /// - `self.dispatch_unassignable_packets(enabled)`
    /// - `self.dispatch_unknown_packets(enabled)`
    /// - `self.dispatch_undecodable_packets(enabled)`
    pub fn dispatch_all_packets(&mut self, enabled: bool) -> &mut Self {
        self.dispatch_unassignable_packets(enabled);
        self.dispatch_unknown_packets(enabled);
        self.dispatch_undecodable_packets(enabled);
        self
    }


    pub fn create(&self) -> Result<PeerConnection, String> {
        let logger = self.logger.clone().ok_or(String::from("missing logger"))?;
        let event_context = self.event_loop.clone().ok_or(String::from("missing event loop"))?;

        if !self.ice_tcp && !self.ice_udp {
            return Err(String::from("at least one ice transport type (tcp, udp) must be activated"));
        }

        if let Some(port_range) = &self.ice_port_range {
            if port_range.0 >= port_range.1 {
                return Err(String::from("invalid ice port range"));
            }
        }

        let mut connection = PeerConnection{
            logger: logger.clone(),

            state: PeerConnectionState::New,
            signalling_state: SignallingState::None,

            peer_poll_waker: None,

            /* "-" indicates no username */
            origin_username: String::from("-"),

            ice_agent: libnice::ice::Agent::new_full(event_context, self.nice_compatibility, self.nice_flags),
            ice_port_range: self.ice_port_range.clone(),

            transport: BTreeMap::new(),

            stream_receiver: BTreeMap::new(),
            stream_sender: BTreeMap::new(),

            allow_application_channel: true,
            application_channel: None,

            media_lines: Vec::new(),
            event_queue: VecDeque::with_capacity(32),

            dispatch_unknown_packets: true,
            dispatch_unassignable_packets: true,
            dispatch_undecodable_packets: true,
        };

        connection.ice_agent.get_ffi_agent().on_selected_pair(move |stream_id, component_id, local_candidate, remote_candidate| {
            slog_debug!(logger, "Changed candidate pair for stream {} (Component: {}). Local candidate: {}. Remote candidate: {}",
                stream_id, component_id, local_candidate.to_sdp().to_string(), remote_candidate.to_sdp().to_string());
        }).unwrap();

        if let Some(stun) = &self.stun {
            connection.ice_agent.get_ffi_agent().set_nice_property(NiceAgentProperty::StunServer(Some(stun.0.clone()))).unwrap();
            connection.ice_agent.get_ffi_agent().set_nice_property(NiceAgentProperty::StunPort(stun.1 as u32)).unwrap();
        }

        connection.ice_agent.get_ffi_agent().set_nice_property(NiceAgentProperty::IceTcp(self.ice_tcp)).unwrap();
        connection.ice_agent.get_ffi_agent().set_nice_property(NiceAgentProperty::IceUdp(self.ice_udp)).unwrap();

        connection.ice_agent.get_ffi_agent().set_nice_property(NiceAgentProperty::Upnp(self.ice_upnp)).unwrap();
        connection.ice_agent.get_ffi_agent().set_nice_property(NiceAgentProperty::ControllingMode(false)).unwrap();
        connection.ice_agent.get_ffi_agent().set_nice_property(NiceAgentProperty::IceTrickle(true)).unwrap();

        Ok(connection)
    }
}

impl PeerConnection {
    pub fn builder() -> PeerConnectionBuilder {
        PeerConnectionBuilder::new()
    }

    fn dispatch_peer_event(&mut self, event: PeerConnectionEvent) {
        self.event_queue.push_back(event);
        if let Some(waker) = &self.peer_poll_waker {
            waker.wake_by_ref()
        }
    }

    /// Create the application channel.
    /// If create_media_line is not set, transport must point to a valid transport!
    fn application_channel(&mut self, create_media_line: bool, mut transport: Option<u32>) -> &mut Option<Box<ChannelApplication>> {
        if !self.application_channel.is_some() && (transport.is_some() || create_media_line) {
            let mut channel = Box::new(ChannelApplication::new(self.logger.new(o!())).expect("failed to allocate new application channel"));
            if create_media_line && self.media_lines.iter().find(|line| RefCell::borrow(line).media_type == SdpMediaValue::Application).is_none() {
                let media_line = self.allocate_media_line(SdpMediaValue::Application);
                transport = Some(RefCell::borrow(&media_line).transport_id);
            }

            let transport = self.transport.get(transport.as_ref().unwrap()).expect("missing media line transport");
            let transport = RefCell::borrow(transport);

            channel.transport_id = transport.transport_id;
            channel.transport = Some(transport.control_sender.clone());

            self.application_channel = Some(channel);
            if let Some(waker) = &self.peer_poll_waker {
                waker.wake_by_ref();
            }
        }
        &mut self.application_channel
    }

    pub fn media_lines(&self) -> &Vec<Rc<RefCell<MediaLine>>> {
        &self.media_lines
    }

    pub fn media_line_by_unique_id(&self, unique_id: u32) -> Option<Rc<RefCell<MediaLine>>> {
        self.media_lines.iter().find(|e| RefCell::borrow(e).unique_id() == unique_id)
            .map(|e| e.clone())
    }

    pub fn media_line_by_sdp_index(&self, index: usize) -> Option<Rc<RefCell<MediaLine>>> {
        self.media_lines.iter().find(|e| RefCell::borrow(e).sdp_index() == &Some(index))
            .map(|e| e.clone())
    }

    pub fn reset(&mut self) {
        self.event_queue.clear();

        self.origin_username = String::from("-");
        self.application_channel = None;
        self.stream_receiver.clear();
        self.stream_sender.clear();
        self.transport.clear();

        self.media_lines.clear();
        self.signalling_state = SignallingState::None;

        self.dispatch_peer_event(PeerConnectionEvent::PeerReset);
    }

    pub fn set_remote_description(&mut self, description: &webrtc_sdp::SdpSession, mode: &RtcDescriptionType) -> Result<(), RemoteDescriptionApplyError> {
        slog_trace!(self.logger, "Received remote session description ({:?})", mode);

        if mode == &RtcDescriptionType::Offer {
            if !matches!(&self.signalling_state, &SignallingState::None | &SignallingState::Negotiated) {
                return Err(RemoteDescriptionApplyError::InvalidNegotiationState);
            }
        } else {
            if !matches!(&self.signalling_state, &SignallingState::HaveLocalOffer) {
                return Err(RemoteDescriptionApplyError::InvalidNegotiationState);
            }
        }

        /* copy the origin username and send it back, required for mozilla for example */
        self.origin_username = description.origin.username.clone();

        for media_line_index in 0..description.media.len() {
            let media = &description.media[media_line_index];

            let credentials = ICECredentials {
                username: get_attribute_value!(media, media_line_index, IceUfrag)?.clone(),
                password: get_attribute_value!(media, media_line_index, IcePwd)?.clone()
            };

            let mut line = self.media_line_by_sdp_index(media_line_index);
            if line.is_none() {
                /*
                 * We've not yet registered a media line for that index.
                 * But if we're already having one, just use it.
                 */
                line = self.media_lines.iter().find_map(|line_ref| {
                    let line = RefCell::borrow(line_ref);
                    if line.sdp_index().is_none() && &line.media_type == media.get_type() {
                        Some(line_ref.clone())
                    } else {
                        None
                    }
                });

                if let Some(line) = &line {
                    let mut line = RefCell::borrow_mut(line);
                    line.set_sdp_index(media_line_index, format!("{}", media_line_index));
                }
            }

            if let Some(line) = line {
                /* we've to update the line */
                let line = line.clone();
                let mut line = RefCell::borrow_mut(&line);

                let transport = self.transport.get(&line.transport_id);
                if transport.is_none() {
                    return Err(RemoteDescriptionApplyError::InternalError { detail: String::from("missing transport for media line") });
                }
                let transport = Rc::clone(&transport.unwrap());
                let mut transport = RefCell::borrow_mut(&transport);

                transport.apply_remote_description(line.unique_id(), media)
                    .map_err(|err| RemoteDescriptionApplyError::IceConfigureError { media_line: media_line_index, error: err })?;

                line.update_from_sdp(media)
                    .map_err(|err| RemoteDescriptionApplyError::MediaLineParseError { media_line: media_line_index, error: err })?;

                if line.media_type == SdpMediaValue::Application {
                    if let Some(channel) = &mut self.application_channel {
                        channel.set_remote_description(media)
                            .map_err(|err| RemoteDescriptionApplyError::MediaChannelConfigure { error: err })?;
                    } else {
                        return Err(RemoteDescriptionApplyError::ApplicationChannelMissing);
                    }
                }

                self.update_media_line_streams(media, line.deref_mut(), transport.deref_mut());
            } else {
                let mut line = MediaLine::new_from_sdp(media_line_index, media)
                    .map_err(|err| RemoteDescriptionApplyError::MediaLineParseError { media_line: media_line_index, error: err })?;

                let transport = {
                    if let Some(transport) = self.transport.values().find(|e| RefCell::borrow(e).remote_credentials() == Some(&credentials)) {
                        transport.clone()
                    } else if let Some(transport) = self.transport.values().find(|e| RefCell::borrow(e).remote_credentials().is_none()) {
                        transport.clone()
                    } else {
                        self.create_transport(line.unique_id())
                            .map_err(|err| RemoteDescriptionApplyError::IceInitializeError { result: err, media_line: media_line_index })?
                    }
                };
                let mut transport_mut = RefCell::borrow_mut(&transport);

                transport_mut.apply_remote_description(line.unique_id(), media)
                    .map_err(|err| RemoteDescriptionApplyError::IceConfigureError { media_line: media_line_index, error: err })?;

                line.transport_id = transport_mut.transport_id;
                transport_mut.media_lines.push(line.unique_id());

                if line.media_type == SdpMediaValue::Application {
                    if self.application_channel.is_none() && !self.allow_application_channel {
                        return Err(RemoteDescriptionApplyError::ApplicationChannelHasBeenDisabled);
                    }

                    let transport_id = Some(transport_mut.transport_id);
                    drop(transport_mut);

                    let application_channel = self.application_channel(false, transport_id).as_mut().unwrap();
                    application_channel.set_remote_description(media)
                        .map_err(|err| RemoteDescriptionApplyError::InternalError { detail: String::from(format!("failed to set remote description: {:?}", err)) })?;

                    /* TODO: Trigger the handle_transport_initialized event if the transport has already been initialized */
                    //self.application_channel.handle_transport_initialized();
                } else {
                    self.update_media_line_streams(media, &mut line, transport_mut.deref_mut());
                }

                self.media_lines.push(Rc::new(RefCell::new(line)));
            }
        }

        /*
         * No need to check here if the lines are contained within the remote description.
         * If they're not contained, their status will never be propagated.
         */
        match &self.signalling_state {
            &SignallingState::HaveLocalOffer => {
                for line in self.media_lines.iter() {
                    let mut line = RefCell::borrow_mut(line);
                    if line.negotiation_state == NegotiationState::Propagated {
                        line.negotiation_state = NegotiationState::Negotiated;
                    }
                }
                for sender in self.stream_sender.values() {
                    RefCell::borrow_mut(sender).promote_negotiation(|s| matches!(s, NegotiationState::Propagated), NegotiationState::Negotiated);
                }

                self.signalling_state = SignallingState::Negotiated;
            },
            &SignallingState::None |
            &SignallingState::Negotiated => {
                self.signalling_state = SignallingState::HaveRemoteOffer;
            },
            _ => panic!()
        }

        slog_trace!(self.logger, "Remote session description applied successfully");
        Ok(())
    }

    fn update_media_line_streams(&mut self, media: &SdpMedia, media_line: &mut MediaLine, transport: &mut RTCTransport) {
        // Replaced btree_drain_filter with stable code
        let keys_to_remove: Vec<_> = self.stream_receiver
            .iter()
            .filter(|(id, receiver)| {
                receiver.track().media_line == media_line.unique_id()
                    && !media_line.remote_streams.contains(id)
            })
            .map(|(id, _)| *id)
            .collect();

        for id in keys_to_remove {
            self.stream_receiver.remove(&id);
        }

        for receiver_id in media_line.remote_streams.iter() {
            if self.stream_receiver.contains_key(&receiver_id) {
                /* TODO: Check if codec or formats have changed and if so fail */
                continue;
            }

            slog_info!(self.logger, "RTP Stream got new {stream} on sdp index {index:?}", stream = receiver_id, index = media_line.sdp_index());
            let (mut internal_receiver, receiver) = ActiveInternalMediaReceiver::new(
                InternalMediaTrack {
                    logger: self.logger.new(o!("id" => *receiver_id)),
                    id: *receiver_id,
                    media_line: media_line.unique_id(),

                    transport_id: media_line.transport_id
                },
                transport.create_rtcp_sender()
            );
            internal_receiver.parse_properties_from_sdp(media);

            self.dispatch_peer_event(PeerConnectionEvent::ReceivedRemoteStream(receiver));
            self.stream_receiver.insert(*receiver_id, Box::new(internal_receiver));
        }

        for sender_id in media_line.local_streams.iter() {
            if let Some(sender) = self.stream_sender.get(sender_id) {
                let sender = RefCell::borrow_mut(sender);
                let mut shared_data = sender.shared_data.lock().unwrap();

                if let Some(_current_codecs) = &shared_data.remote_codecs {
                    /* TODO: Test for changed (The media line already validates the codec change previously) */
                    shared_data.remote_codecs = Some(media_line.remote_codecs.clone());
                    let _ = sender.events.send(MediaSenderEvent::RemoteCodecsUpdated);
                } else {
                    shared_data.remote_codecs = Some(media_line.remote_codecs.clone());
                    let _ = sender.events.send(MediaSenderEvent::RemoteCodecsUpdated);
                }
            } else {
                /* well that should not happen... */
            }
        }
    }

    pub fn create_local_description(&mut self) -> Result<SdpSession, CreateAnswerError> {
        if matches!(&self.signalling_state, &SignallingState::HaveLocalOffer | &SignallingState::Negotiated) {
            return Err(CreateAnswerError::InvalidNegotiationState);
        }
        slog_trace!(self.logger, "Generating local session description (Signalling state: {:?})", self.signalling_state);

        let is_offer = self.signalling_state != SignallingState::HaveRemoteOffer;
        if is_offer {
            /* We're doing an offer. Assigning ID's to all pending media lines */
            let mut current_line_index = self.media_lines.iter()
                .map(|e| RefCell::borrow(e).sdp_index().map_or(0, |e| e + 1)).max()
                .unwrap_or(0);

            for line in self.media_lines.iter_mut() {
                let mut line = RefCell::borrow_mut(line);
                if line.sdp_index().is_none() {
                    line.set_sdp_index(current_line_index, format!("{}", current_line_index));
                    current_line_index += 1;
                }
            }
        }

        /* flush all pending stream modifications */
        self.poll_stream_receiver(|receiver| receiver.flush_control());
        self.poll_stream_sender(|sender| RefCell::borrow_mut(sender).flush_control());

        let mut registered_media_lines = self.media_lines.iter()
            .filter_map(|e| RefCell::borrow(e).sdp_index().map(|index| (index, e.clone())))
            .collect::<Vec<_>>();
        registered_media_lines.sort_by_key(|e| e.0);

        let mut answer = SdpSession::new(0, SdpOrigin {
            session_id: rand::random::<u64>() & 0x7FFF_FFFF_FFFF_FFFF,
            session_version: 2,
            unicast_addr: ExplicitlyTypedAddress::Ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            username: self.origin_username.clone()
        }, String::from("-")); /* "-" indicates no session id */
        answer.timing = Some(SdpTiming{ start: 0, stop: 0 }); /* required by WebRTC */

        /* Bundle all media streams */
        answer.add_attribute(SdpAttribute::Group(SdpAttributeGroup{
            semantics: SdpAttributeGroupSemantic::Bundle,
            tags: registered_media_lines.iter().map(|e| RefCell::borrow(&e.1).media_id().clone()).filter_map(|e| e).collect::<Vec<_>>()
        })).unwrap();

        for (sdp_index, media_line) in registered_media_lines.iter() {
            let media_line = RefCell::borrow(media_line);
            let mut media = {
                if media_line.media_type == SdpMediaValue::Application {
                    if let Some(channel) = &self.application_channel {
                        let mut media = channel.generate_local_description()
                            .map_err(|err| CreateAnswerError::InternalError(err))?;

                        if let Some(media_id) = media_line.media_id() {
                            media.add_attribute(SdpAttribute::Mid(media_id.clone()))
                                .unwrap();
                        }
                        media
                    } else {
                        return Err(CreateAnswerError::InternalError(String::from("having an application channel media line, but no application channel handle")));
                    }
                } else {
                    let mut media = media_line.generate_local_description(is_offer)
                        .ok_or(CreateAnswerError::DescribeError(*sdp_index))?;

                    for sender in self.stream_sender.values()
                        .filter(|sender| RefCell::borrow(sender).track.media_line == media_line.unique_id()) {
                        RefCell::borrow_mut(sender).write_sdp(&mut media);
                    }

                    media
                }
            };

            let ice_channel = self.transport.get(&media_line.transport_id);
            if ice_channel.is_none() {
                return Err(CreateAnswerError::MissingTransportChannel(*sdp_index));
            }
            let ice_channel = RefCell::borrow(ice_channel.unwrap());

            /* generating a local description should never fail */
            ice_channel.generate_local_description(&mut media).unwrap();
            answer.media.push(media);
        }

        match &self.signalling_state {
            &SignallingState::None |
            &SignallingState::NegotiationRequired => {
                for (_, line) in registered_media_lines.iter() {
                    let mut line = RefCell::borrow_mut(line);
                    if line.negotiation_state == NegotiationState::Changed ||
                        line.negotiation_state == NegotiationState::None {
                        line.negotiation_state = NegotiationState::Propagated;
                    }
                }

                for sender in self.stream_sender.values() {
                    let media_line = RefCell::borrow(sender).track.media_line;
                    if self.media_line_by_unique_id(media_line).map(|e| RefCell::borrow(&e).sdp_index().clone()).is_some() {
                        RefCell::borrow_mut(sender).promote_negotiation(|s| matches!(s, NegotiationState::Changed | NegotiationState::None), NegotiationState::Propagated);
                    }
                }
                self.signalling_state = SignallingState::HaveLocalOffer;
            },
            &SignallingState::HaveRemoteOffer => {
                for (_, line) in registered_media_lines.iter() {
                    let mut line = RefCell::borrow_mut(line);
                    line.negotiation_state = NegotiationState::Negotiated;
                }

                for sender in self.stream_sender.values() {
                    let media_line = RefCell::borrow(sender).track.media_line;
                    if self.media_line_by_unique_id(media_line).map(|e| RefCell::borrow(&e).sdp_index().clone()).is_some() {
                        RefCell::borrow_mut(sender).promote_negotiation(|_| true, NegotiationState::Negotiated);
                    }
                }

                self.signalling_state = SignallingState::Negotiated;
            }
            _ => panic!() /* this other cases should never happen, they're already caught in the first few lines */
        }

        slog_trace!(self.logger, "Local session description has been generated.");
        Ok(answer)
    }

    /// Return the number of free media lines which are not yet in use by a sender.
    pub fn free_send_media_lines(&self, media_type: SdpMediaValue) -> usize {
        self.media_lines.iter()
            .map(|e| RefCell::borrow(e))
            .filter(|e| e.media_type == media_type && e.local_streams.is_empty())
            .count()
    }

    /// Allocate a new media line.
    /// Attention: This will not wake the peer poll waker it will not be directly negotiated!
    fn allocate_media_line(&mut self, media_type: SdpMediaValue) -> Rc<RefCell<MediaLine>> {
        for line in self.media_lines.iter() {
            let ref_line = RefCell::borrow(line);
            if ref_line.media_type != media_type { continue; }
            if !ref_line.local_streams.is_empty() { continue; }
            return line.clone();
        }

        let mut line = MediaLine::new(media_type);
        let transport = if self.transport.is_empty() {
            /* Should not fail, else we've some serious issues */
            self.create_transport(line.unique_id()).unwrap()
        } else {
            self.transport.first_key_value().unwrap().1.clone()
        };
        let transport = RefCell::borrow(&transport);
        line.transport_id = transport.transport_id;

        let line = Rc::new(RefCell::new(line));
        self.media_lines.push(line.clone());
        return line;
    }

    pub fn create_media_sender(&mut self, media_type: SdpMediaValue) -> MediaSender {
        /* find or create a free media line */
        let media_line = self.allocate_media_line(media_type);
        let mut media_line = RefCell::borrow_mut(&media_line);

        let mut ssrc = rand::random::<u32>();
        while self.stream_sender.contains_key(&ssrc) || self.stream_receiver.contains_key(&ssrc) {
            ssrc += 1;
        }

        let mut transport = RefCell::borrow_mut(self.transport.get(&media_line.transport_id).expect("missing transport"));
        let (internal_sender, sender) = InternalMediaSender::new(
            InternalMediaTrack {
                logger: self.logger.new(o!("id" => ssrc)),
                id: ssrc,
                media_line: media_line.unique_id(),

                transport_id: media_line.transport_id
            },
            (transport.create_rtp_sender(), transport.create_rtcp_sender())
        );

        media_line.local_streams.push(internal_sender.track.id);
        internal_sender.shared_data.lock().unwrap().remote_codecs = Some(media_line.remote_codecs.clone());

        self.stream_sender.insert(internal_sender.track.id, RefCell::new(internal_sender));

        if let Some(waker) = &self.peer_poll_waker {
            waker.wake_by_ref();
        }
        sender
    }

    pub fn create_data_channel(&mut self, channel_type: DataChannelType, label: String, protocol: Option<String>, priority: u16) -> Result<DataChannel, String> {
        if !self.allow_application_channel {
            Err(String::from("application channels have been disabled"))
        } else {
            let channel = self.application_channel(true, None).as_mut().unwrap();
            channel.create_data_channel(channel_type, label, protocol, priority)
        }
    }

    /// Adding a remote ice candidate.
    /// To signal a no more candidates event just add `None`
    pub fn add_remote_ice_candidate(&mut self, media_line_index: usize, candidate: Option<&Candidate>) -> Result<(), RTCTransportICECandidateAddError> {
        if let Some(media_line) = self.media_line_by_sdp_index(media_line_index) {
            let media_line = RefCell::borrow(&media_line);

            let ice_transport = self.transport.get_mut(&media_line.transport_id);
            if let Some(transport) = ice_transport {
                let mut transport = RefCell::borrow_mut(transport);

                if transport.owning_media_line == media_line.unique_id() {
                    transport.add_remote_candidate(candidate)
                } else {
                    Ok(())
                }
            } else {
                Err(RTCTransportICECandidateAddError::MissingTransport)
            }
        } else {
            Err(RTCTransportICECandidateAddError::UnknownMediaChannel)
        }
    }

    fn create_transport(&mut self, owning_media_line: u32) -> Result<Rc<RefCell<RTCTransport>>, RTCTransportInitializeError> {
        slog_debug!(self.logger, "Creating a new transport channel (Owner: {})", owning_media_line);

        /* register a new channel */
        let mut stream = libnice::ice::Agent::stream_builder(&mut self.ice_agent, 1);
        if let Some(port_range) = &self.ice_port_range {
            stream.set_port_range(port_range.0, port_range.1);
        }
        let stream = stream
            .set_inbound_buffer_size(1024 * 8)
            .build()
            .map_err(|error| RTCTransportInitializeError::IceStreamAllocationFailed { error })?;

        #[allow(unused_mut)]
        let mut connection = RTCTransport::new(stream, owning_media_line, &self.logger)?;

        /* FIXME! */
        #[cfg(feature = "simulated-loss")]
        {
            //connection.set_simulated_loss(10);
        }

        let id = connection.transport_id;
        let owning_media_line = connection.owning_media_line;

        let connection = Rc::new(RefCell::new(connection));
        self.transport.insert(id, connection.clone());

        slog_trace!(self.logger, "Transport channel {} has been created. Owner: {}", id, owning_media_line);
        Ok(connection)
    }

    fn find_ice_channel_by_media_fragment(&self, line_unique_id: u32) -> Option<&Rc<RefCell<RTCTransport>>> {
        self.transport.iter().find(|channel| RefCell::borrow(channel.1).media_lines.iter().find(|media| **media == line_unique_id).is_some())
            .map(|e| e.1)
    }

    fn handle_ice_event(&mut self, ice: &mut RefMut<RTCTransport>, event: RTCTransportEvent) {
        match event {
            RTCTransportEvent::LocalIceCandidate(candidate) => {
                let media_line = self.media_line_by_unique_id(ice.owning_media_line);
                if let Some(Some(index)) = media_line.as_ref().map(|e| RefCell::borrow(e).sdp_index().clone()) {
                    self.dispatch_peer_event(PeerConnectionEvent::LocalIceCandidate(Some(candidate.into()), index));
                } else {
                    /* We received an ICE candidate for an not yet registered media line */
                }
            },
            RTCTransportEvent::LocalIceGatheringFinished() => {
                let media_line = self.media_line_by_unique_id(ice.owning_media_line);
                if let Some(Some(index)) = media_line.map(|e| RefCell::borrow(&e).sdp_index().clone()) {
                    self.dispatch_peer_event(PeerConnectionEvent::LocalIceCandidate(None, index));
                } else {
                    /* We received an ICE candidate gathering finished signal for an not yet registered media line */
                }
            },
            RTCTransportEvent::TransportStateChanged => {
                /* TODO: Improve state change handling. This specially applies to transport failures */
                slog_debug!(self.logger, "Transport channel {} changed its state to {:?}", ice.transport_id, ice.state());

                match ice.state() {
                    &RTCTransportState::Connected => {
                        if let Some(channel) = &mut self.application_channel {
                            channel.handle_transport_connected();
                        }
                    },
                    _ => {}
                }
            },
            RTCTransportEvent::MessageReceivedDtls(message) => {
                if let Some(channel) = &mut self.application_channel {
                    if channel.transport_id == ice.transport_id {
                        channel.handle_data(message);
                    } else if self.dispatch_unassignable_packets {
                        self.dispatch_peer_event(PeerConnectionEvent::UnassignableDtlsPacket(message));
                    }
                } else if self.dispatch_unassignable_packets {
                    self.dispatch_peer_event(PeerConnectionEvent::UnassignableDtlsPacket(message));
                }
            },
            RTCTransportEvent::MessageReceivedRtcp(message) => {
                let mut packets = [&[0u8][..]; 128];
                let packet_count = RtcpPacket::split_up_packets(message.as_slice(), &mut packets[..]);
                if let Err(error) = packet_count {
                    if self.dispatch_undecodable_packets {
                        self.dispatch_peer_event(PeerConnectionEvent::UndecodableRtcpPacket(message, error));
                    }
                    return;
                }

                for index in 0..packet_count.unwrap() {
                    match RtcpPacket::parse(packets[index]) {
                        Ok(packet) => {
                            /*
                            let mut buffer = [0u8; 2038];
                            match packet.write(&mut buffer) {
                                Err(error) => {
                                    eprintln!("Failed to write received RTCP packet: {:?}", error);
                                },
                                Ok(length) => {
                                    if packets[index] != &buffer[0..length] {
                                        /*
                                            FF Pads the SourceDescription elements invalid (https://bugzilla.mozilla.org/show_bug.cgi?id=1671169).
                                            Example: The CName is 38 characters long adding two (one byte for the description type, the other for the length) results in a length of 40.
                                            40 has no need to be padded (already on a 32bit boundary). For some reason FF padds the message with two bytes. This results later on in the padding of four zero bytes and result in an over all invalid packet
                                            Parsed packet: SourceDescription(RtcpPacketSourceDescription { descriptions: [(1308369285, CName("{4b1d1d86-d4bc-44a2-a92d-6e47ee1ed6a3}"))] })
                                            Created packet is different than source packet:
                                            Source:  [129, 202, 0, 12, 77, 252, 33, 133, 1, 38, 123, 52, 98, 49, 100, 49, 100, 56, 54, 45, 100, 52, 98, 99, 45, 52, 52, 97, 50, 45, 97, 57, 50, 100, 45, 54, 101, 52, 55, 101, 101, 49, 101, 100, 54, 97, 51, 125, 0, 0, 0, 0]
                                            Created: [129, 202, 0, 12, 77, 252, 33, 133, 1, 38, 123, 52, 98, 49, 100, 49, 100, 56, 54, 45, 100, 52, 98, 99, 45, 52, 52, 97, 50, 45, 97, 57, 50, 100, 45, 54, 101, 52, 55, 101, 101, 49, 101, 100, 54, 97, 51, 125]
                                         */
                                        eprintln!("Parsed packet: {:?}", packet);
                                        eprintln!("Created packet is different than source packet:\nSource:  {:?}\nCreated: {:?}", &packets[index], &buffer[0..length]);
                                    }
                                }
                            }
                            */

                            match packet {
                                RtcpPacket::ReceiverReport(mut rr) => {
                                    let app_data = rr.profile_data.take();
                                    for (id, report) in rr.reports {
                                        if let Some(sender) = self.stream_sender.get(&id) {
                                            RefCell::borrow_mut(sender).handle_receiver_report(report, &app_data);
                                        }
                                    }
                                },
                                RtcpPacket::SenderReport(sr) => {
                                    if let Some(receiver) = self.stream_receiver.get_mut(&sr.ssrc) {
                                        receiver.handle_sender_report(sr);
                                    } else if self.dispatch_unassignable_packets {
                                        self.dispatch_peer_event(PeerConnectionEvent::UnassignableRtcpPacket(RtcpPacket::SenderReport(sr)));
                                    }
                                },
                                RtcpPacket::SourceDescription(sd) => {
                                    for (id, description) in sd.descriptions.iter() {
                                        if let Some(receiver) = self.stream_receiver.get_mut(&id) {
                                            receiver.handle_source_description(description);
                                        }
                                    }
                                },
                                RtcpPacket::Bye(bye) => {
                                    for ssrc in bye.src.iter() {
                                        if let Some(receiver) = self.stream_receiver.get_mut(ssrc) {
                                            receiver.handle_bye(&bye.reason);
                                        }
                                    }
                                },
                                RtcpPacket::TransportFeedback(fb) => {
                                    if let Some(sender) = self.stream_sender.get(&fb.media_ssrc) {
                                        RefCell::borrow_mut(sender).handle_transport_feedback(fb.feedback);
                                    } else if self.dispatch_unassignable_packets {
                                        self.dispatch_peer_event(PeerConnectionEvent::UnassignableRtcpPacket(RtcpPacket::TransportFeedback(fb)));
                                    }
                                },
                                RtcpPacket::PayloadFeedback(pfb) => {
                                    if let Some(sender) = self.stream_sender.get(&pfb.media_ssrc) {
                                        RefCell::borrow_mut(sender).handle_payload_feedback(pfb.feedback);
                                    } else if self.dispatch_unassignable_packets {
                                        self.dispatch_peer_event(PeerConnectionEvent::UnassignableRtcpPacket(RtcpPacket::PayloadFeedback(pfb)));
                                    }
                                },
                                RtcpPacket::ExtendedReport(xr) => {
                                    /* TODO: What to do here? We can't really assign the report to any media sender/receiver... */
                                    /*
                                    if let Some(sender) = self.stream_sender.get_mut(&xr.ssrc) {
                                        sender.handle_extended_report(xr);
                                    } else if let Some(receiver) = self.stream_receiver.get_mut(&xr.ssrc) {
                                        receiver.handle_extended_report(xr);
                                    } else {
                                        let _ = self.local_events.0.send(PeerConnectionEvent::UnassignableRtcpPacket(RtcpPacket::ExtendedReport(xr)));
                                    }
                                    */
                                    if self.dispatch_unassignable_packets {
                                        self.dispatch_peer_event(PeerConnectionEvent::UnassignableRtcpPacket(RtcpPacket::ExtendedReport(xr)));
                                    }
                                },
                                RtcpPacket::Unknown(data) => {
                                    if self.dispatch_unknown_packets {
                                        self.stream_receiver.iter_mut().for_each(|receiver|
                                            receiver.1.handle_unknown_rtcp(&data)
                                        );

                                        self.stream_sender.iter().for_each(|sender|
                                            RefCell::borrow_mut(sender.1).handle_unknown_rtcp(&data)
                                        );
                                    }
                                }
                                _ => {}
                            }
                        },
                        Err(error) => {
                            if self.dispatch_undecodable_packets {
                                self.dispatch_peer_event(PeerConnectionEvent::UndecodableRtcpPacket(packets[index].to_owned(), error));
                            }
                        }
                    }
                }
            },
            RTCTransportEvent::MessageReceivedRtp(message) => {
                match ParsedRtpPacket::new(message) {
                    Ok(reader) => {
                        if let Some(receiver) = self.stream_receiver.get_mut(&reader.ssrc()) {
                            if receiver.track().transport_id != ice.transport_id {
                                slog_warn!(self.logger, "Received RTP message for receiver, but receiver isn't registered to that transport. Expected {}, Received: {}", receiver.track().transport_id, ice.transport_id);
                            } else {
                                receiver.handle_rtp_packet(reader);
                            }
                        } else if self.dispatch_unassignable_packets {
                            self.dispatch_peer_event(PeerConnectionEvent::UnassignableRtpPacket(reader));
                        }
                    },
                    Err((error, message)) => {
                        if self.dispatch_undecodable_packets {
                            self.dispatch_peer_event(PeerConnectionEvent::UndecodableRtpPacket(message, error));
                        }
                    }
                }
            },
            RTCTransportEvent::MessageDropped(message) => {
                /* This is for internal debug only */
                slog_trace!(self.logger, "Dropping received ICE message of length {}", message.len());
            },
            _ => {
                /* XXX: Explicitly handle all events? */
            }
        }

    }

    fn poll_stream_receiver<F>(&mut self, poll_fn: F)
        where F: Fn(&mut Box<dyn InternalMediaReceiver>) -> bool
    {
        // Replaced btree_drain_filter with stable code
        let keys_to_process: Vec<_> = self.stream_receiver
            .iter_mut()
            .filter(|(_, receiver)| poll_fn(receiver))
            .map(|(id, _)| *id)
            .collect();

        let removed: Vec<_> = keys_to_process
            .iter()
            .filter_map(|id| self.stream_receiver.remove(id).map(|v| (*id, v)))
            .collect();

        for (id, rc) in removed {
            self.stream_receiver.insert(id, rc.into_void());
            slog_debug!(self.logger, "Media receiver for channel {} has been closed. Using void receiver.", id);
        }
    }

    fn poll_stream_sender<F>(&mut self, poll_fn: F)
        where F: Fn(&mut RefCell<InternalMediaSender>) -> bool
    {
        // Replaced btree_drain_filter with stable code
        let keys_to_remove: Vec<_> = self.stream_sender
            .iter_mut()
            .filter(|(_, sender)| poll_fn(sender))
            .map(|(id, _)| *id)
            .collect();

        let removed: Vec<_> = keys_to_remove
            .iter()
            .filter_map(|id| self.stream_sender.remove(id).map(|v| (*id, v)))
            .collect();

        for (_, sender) in removed {
            let sender = RefCell::borrow(&sender);
            let media_line = self.media_line_by_unique_id(sender.track.media_line);
            if let Some(media_line) = media_line {
                let mut media_line = RefCell::borrow_mut(&media_line);
                media_line.local_streams.retain(|e| *e != sender.track.id);

                /* Change the state, even if the media line has been removed. Should have no impact. */
                if media_line.negotiation_state != NegotiationState::None {
                    media_line.negotiation_state = NegotiationState::Changed;
                }

                if media_line.local_streams.is_empty() && media_line.remote_streams.is_empty() && media_line.sdp_index().is_none() {
                    /* safely remove that line */
                    let unique_id = media_line.unique_id();
                    drop(media_line); /* drop our mutable reference so we can borrow it imutable */
                    if let Some(index) = self.media_lines.iter().position(|e| RefCell::borrow(e).unique_id() == unique_id) {
                        /* fully remove that media line, no need to keep it */
                        self.media_lines.remove(index);
                    }
                }
            }
        }
    }
}

unsafe impl Send for PeerConnection {}

impl futures::stream::Stream for PeerConnection {
    type Item = PeerConnectionEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.peer_poll_waker = Some(cx.waker().clone());

        /* firstly process all pending events before generating new once */
        if let Some(message) = self.event_queue.pop_front() {
            return Poll::Ready(Some(message));
        }

        self.poll_stream_receiver(|receiver| if let Poll::Ready(_) = receiver.poll_unpin(&mut Context::from_waker(cx.waker())) { true } else { false });
        self.poll_stream_sender(|sender| if let Poll::Ready(_) = RefCell::borrow_mut(sender).poll_unpin(&mut Context::from_waker(cx.waker())) { true } else { false });

        if let Some(channel) = &mut self.application_channel {
            while let Poll::Ready(event) = channel.poll_next_unpin(cx) {
                let event = event.expect("unexpected stream end");
                match event {
                    ApplicationChannelEvent::DataChannelReceived(channel) => {
                        return Poll::Ready(Some(PeerConnectionEvent::ReceivedDataChannel(channel)));
                    },
                    ApplicationChannelEvent::StateChanged { new_state: _ } => {
                        /* TODO: Track the application channel state */
                    }
                }
            }
        }

        let streams = self.transport.clone();
        for (_, stream) in streams.iter() {
            let mut stream = RefCell::borrow_mut(stream);
            while let Poll::Ready(event) = stream.poll_next_unpin(cx) {
                if let Some(event) = event {
                    self.handle_ice_event(&mut stream, event);
                } else {
                    /* TODO: It's not unexpected if receive some kind of error previously. We need some error handing beforehand */
                    panic!("Unexpected ICE exit");
                }
            }
        }

        let _ = self.ice_agent.poll_unpin(cx);

        if self.signalling_state == SignallingState::Negotiated {
            let mut negotiation_required = false;
            let changed_mline = self.media_lines.iter()
                .find(|e| matches!(RefCell::borrow(e).negotiation_state(), NegotiationState::None | NegotiationState::Changed));
            if changed_mline.is_some() {
                negotiation_required = true;
            }
            if !negotiation_required && self.stream_sender.values()
                .find(|e| RefCell::borrow(e).negotiation_needed()).is_some() {
                negotiation_required = true;
            }
            if negotiation_required {
                self.signalling_state = SignallingState::NegotiationRequired;
                return Poll::Ready(Some(PeerConnectionEvent::NegotiationNeeded));
            }
        }

        /* The actions above may have invoked some events. If so return them. */
        if let Some(message) = self.event_queue.pop_front() {
            return Poll::Ready(Some(message));
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod test {
    use crate::utils::rtcp::RtcpPacket;

    const FIREFOX_PACKET: [u8; 52] = [129, 202, 0, 12, 77, 252, 33, 133, 1, 38, 123, 52, 98, 49, 100, 49, 100, 56, 54, 45, 100, 52, 98, 99, 45, 52, 52, 97, 50, 45, 97, 57, 50, 100, 45, 54, 101, 52, 55, 101, 101, 49, 101, 100, 54, 97, 51, 125, 0, 0, 0, 0];

    #[test]
    fn test_packet_split_up() {
        let mut packets = [&[0u8][..]; 128];
        let packet_count = RtcpPacket::split_up_packets(&FIREFOX_PACKET[..], &mut packets[..]);
        assert_eq!(packet_count.unwrap(), 1usize);
    }

    #[test]
    fn test_packet_parse() {
        let parsed = RtcpPacket::parse(&FIREFOX_PACKET[..]).expect("failed to decode valid packet");
        println!("{:?}", parsed);
    }
}