use e2d2::operators::{ReceiveBatch, Batch, merge, merge_with_selector,};
use e2d2::scheduler::{Executable, Runnable, Scheduler, StandaloneScheduler};
use e2d2::allocators::CacheAligned;
use e2d2::headers::{IpHeader, MacHeader, TcpHeader};
use e2d2::interface::*;
use e2d2::utils::ipv4_extract_flow;
use e2d2::queues::{new_mpsc_queue_pair};
use e2d2::headers::EndOffset;
use e2d2::utils;

use std::cmp::min;
use std::sync::mpsc::{Sender, channel};
use std::sync::Arc;
use std::net::{SocketAddrV4, Ipv4Addr};
use std::collections::HashMap;

use uuid::Uuid;
use bincode::{serialize};

use cmanager::{TcpState, TcpControls, TcpCounter, Connection, L234Data, ConnectionManager, ReleaseCause};
use EngineConfig;
use is_kni_core;
use {PipelineId, MessageFrom, MessageTo, TaskType, Timeouts};
use tasks::{PRIVATE_ETYPE_PACKET, PRIVATE_ETYPE_TIMER, PacketInjector, TickGenerator};
use timer_wheel::{MILLIS_TO_CYCLES, TimerWheel};
use std::sync::atomic::Ordering;

const MIN_FRAME_SIZE: usize = 60; // without fcs

#[allow(dead_code)]
#[allow(unused_variables)]
pub fn setup_generator(
    core: i32,
    nr_connections: usize, //# of connections to setup per pipeline
    pci: &CacheAligned<PortQueue>,
    kni: &CacheAligned<PortQueue>,
    sched: &mut StandaloneScheduler,
    engine_config: &EngineConfig,
    servers: Vec<L234Data>,
    flowdirector_map: HashMap<i32, Arc<FlowDirector>>,
    tx: Sender<MessageFrom>,
) {
    fn install_task<T: Executable + 'static>(
        sched: &mut StandaloneScheduler,
        tx: &Sender<MessageFrom>,
        pipeline_id: &PipelineId,
        task_name: &str,
        task_type: TaskType,
        task: T,
    ) -> Uuid {
        let uuid = Uuid::new_v4();
        sched.add_runnable(Runnable::from_task(uuid, task_name.to_string(), task).move_unready());
        tx.send(MessageFrom::Task(pipeline_id.clone(), uuid, task_type)).unwrap();
        uuid
    }

    let mut me = engine_config.get_l234data();
    let l4flow_for_this_core = flowdirector_map.get(&pci.port.port_id()).unwrap().get_flow(pci.rxq());
    me.ip=l4flow_for_this_core.ip; // in case we use destination IP address for flow steering

    let pipeline_id = PipelineId {
        core: core as u16,
        port_id: pci.port.port_id() as u16,
        rxq: pci.rxq(),
    };
    debug!("enter setup_generator {}", pipeline_id);

    let mut sm: ConnectionManager = ConnectionManager::new(
        pipeline_id.clone(),
        pci.clone(),
        &engine_config,
        l4flow_for_this_core,
        tx.clone());

    let timeouts = Timeouts::default_or_some(&engine_config.timeouts);
    let mut wheel = TimerWheel::new(128, 100 * MILLIS_TO_CYCLES, 128);
    debug!("{} wheel cycle= {} millis", pipeline_id, wheel.get_max_timeout_cycles()/MILLIS_TO_CYCLES);

    // setting up a a reverse message channel between this pipeline and the main program thread
    debug!("{} setting up reverse channel", pipeline_id);
    let (remote_tx, rx) = channel::<MessageTo>();
    // we send the transmitter to the remote receiver of our messages
    tx.send(MessageFrom::Channel(pipeline_id.clone(), remote_tx)).unwrap();

    // forwarding frames coming from KNI to PCI, if we are the kni core
    if is_kni_core(pci) {
        let forward2pci = ReceiveBatch::new(kni.clone())
            .parse::<MacHeader>()
            //.transform(box move |p| {
            //    let ethhead = p.get_mut_header();
            //    //debug!("sending KNI frame to PCI: Eth header = { }", &ethhead);
            //})
            .send(pci.clone());
        let uuid = Uuid::new_v4();
        let name = String::from("Kni2Pci");
        sched.add_runnable(Runnable::from_task(uuid, name, forward2pci).move_ready());
    }
    let thread_id_0 = format!("<c{}, rx{}>: ", core, pci.rxq());
    let thread_id_1 = format!("<c{}, rx{}>: ", core, pci.rxq());
    let thread_id_2 = format!("<c{}, rx{}>: ", core, pci.rxq());

    let me_clone = me.clone();
    // only accept traffic from PCI with matching L2 address
    let l2filter_from_pci = ReceiveBatch::new(pci.clone()).parse::<MacHeader>().filter(box move |p| {
        let header = p.get_header();
        if header.dst == me_clone.mac {
            //debug!("{} from pci: found mac: {} ", thread_id_0, &header);
            true
        } else if header.dst.is_multicast() || header.dst.is_broadcast() {
            //debug!("{} from pci: multicast mac: {} ", thread_id_0, &header);
            true
        } else {
            debug!("{} from pci: discarding because mac unknown: {} ", thread_id_0, &header);
            false
        }
    });

    let tcp_min_port = sm.tcp_port_base();
    let pd_clone = me.clone();
    let uuid_l2groupby = Uuid::new_v4();
    let uuid_l2groupby_clone = uuid_l2groupby.clone();
    let pipeline_ip=sm.ip();
    // group the traffic into TCP traffic addressed to Proxy (group 1),
    // and send all other traffic to KNI (group 0)
    let mut l2groups = l2filter_from_pci.group_by(
        2,
        box move |p| {
            if p.get_header().etype() != 0x0800 {
                // everything other than Ipv4 we send to KNI
                return 0
            }
            let payload = p.get_payload();
            let ipflow = ipv4_extract_flow(payload);
            if (ipflow.dst_ip == pd_clone.ip || ipflow.dst_ip == pipeline_ip) && ipflow.proto == 6 {
                if ipflow.dst_port == pd_clone.port || ipflow.dst_port >= tcp_min_port {
                    //debug!("{} our tcp flow: {}", thread_id_1, ipflow);
                    1
                } else {
                    //debug!("{} not our tcp flow: {}", thread_id_1, ipflow);
                    0
                }
            } else {
                debug!("{} unexpected IP packet, sending to KNI: {}, dest-ip= {}, ip assigned to core = {}, proto= {}",
                       thread_id_1,
                       p.get_header(),
                       Ipv4Addr::from(ipflow.dst_ip),
                       Ipv4Addr::from(pipeline_ip),
                       ipflow.proto,
                );
                0
            }
        },
        sched,
        uuid_l2groupby_clone,
    );

    // we create SYN packets and merge them with the upstream coming from the pci i/f
    let tx_clone = tx.clone();
    let (producer, consumer) = new_mpsc_queue_pair();
    //    let creator = PacketInjector::new(producer, &me, no_packets, pipeline_id.clone(), tx_clone.clone());
    let creator = PacketInjector::new(producer, &me, 0);

    let mut counter_to = TcpCounter::new();
    let mut counter_from = TcpCounter::new();

    let injector_uuid=install_task(sched, &tx, &pipeline_id, "PacketInjector", TaskType::TcpGenerator, creator);
    let injector_ready_flag = sched.get_ready_flag(&injector_uuid).unwrap();

    // set up the generator producing timer tick packets with our private EtherType
    let (producer_timerticks, consumer_timerticks) = new_mpsc_queue_pair();
    //TODO calculate tick_length
    let tick_generator = TickGenerator::new(producer_timerticks, &me, 22700);
    assert!(wheel.resolution() > tick_generator.tick_length());
    let wheel_tick_reduction_factor = wheel.resolution() / tick_generator.tick_length();
    let mut ticks = 0;
    install_task(
        sched,
        &tx,
        &pipeline_id,
        "TickGenerator",
        TaskType::TickGenerator,
        tick_generator,
    );

    let pipeline_id_clone = pipeline_id.clone();

    let l2_input_stream = merge_with_selector(
        vec![
            consumer.compose(),
            consumer_timerticks.compose(),
            l2groups.get_group(1).unwrap().compose(),
        ],
        vec![0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 1],
    );
    // group 0 -> dump packets
    // group 1 -> send to PCI
    // group 2 -> send to KNI
    let uuid_l4groupby = Uuid::new_v4();
    let uuid_l4groupby_clone = uuid_l4groupby.clone();
    // process TCP traffic addressed to Proxy
    let mut l4groups = l2_input_stream
        .parse::<MacHeader>()
        .parse::<IpHeader>()
        .parse::<TcpHeader>()
        .group_by(
            3,
            box move |p| {
                // this is the major closure for TCP processing

                let injector_start = || {
                    trace!("{} (re-)starting the injector", thread_id_2);
                    injector_ready_flag.store(true, Ordering::SeqCst);
                };

                let injector_stop = || {
                    trace!("{}: stopping the injector", thread_id_2);
                    injector_ready_flag.store(false, Ordering::SeqCst);
                };

                let injector_runs = || injector_ready_flag.load(Ordering::SeqCst);

                struct HeaderState<'a> {
                    mac: &'a mut MacHeader,
                    ip: &'a mut IpHeader,
                    tcp: &'a mut TcpHeader,
                    //flow: Flow,
                }

                impl<'a> HeaderState<'a> {
                    #[inline]
                    fn set_server_socket(&mut self, ip: u32, port: u16) {
                        self.ip.set_dst(ip);
                        self.tcp.set_dst_port(port);
                    }
                }

                fn do_ttl(h: &mut HeaderState) {
                    let ttl = h.ip.ttl();
                    if ttl >= 1 {
                        h.ip.set_ttl(ttl - 1);
                    }
                    h.ip.update_checksum();
                }

                fn make_reply_packet(h: &mut HeaderState) {
                    let smac = h.mac.src;
                    let dmac = h.mac.dst;
                    let sip = h.ip.src();
                    let dip = h.ip.dst();
                    let sport = h.tcp.src_port();
                    let dport = h.tcp.dst_port();
                    h.mac.set_smac(&dmac);
                    h.mac.set_dmac(&smac);
                    h.ip.set_dst(sip);
                    h.ip.set_src(dip);
                    h.tcp.set_src_port(dport);
                    h.tcp.set_dst_port(sport);
                    h.tcp.set_ack_flag();
                    let ack_num = h.tcp.seq_num().wrapping_add(1);
                    h.tcp.set_ack_num(ack_num);
                }

                #[inline]
                fn set_header(server: &L234Data, port: u16, h: &mut HeaderState, me: &L234Data) {
                    h.mac.set_dmac(&server.mac);
                    h.mac.set_smac(&me.mac);
                    h.set_server_socket(server.ip, server.port);
                    h.ip.set_src(me.ip);
                    h.tcp.set_src_port(port);
                    h.ip.update_checksum();
                }
                /*
                #[inline]
                pub fn tcpip_payload_size<M: Sized + Send>(p: &Packet<TcpHeader, M>) -> u16 {
                    let iph = p.get_pre_header().unwrap();
                    // payload size = ip total length - ip header length -tcp header length
                    iph.length() - (iph.ihl() as u16) * 4u16 - (p.get_header().data_offset() as u16) * 4u16
                }
                */

                // remove tcp options for SYN and SYN-ACK,
                // pre-requisite: no payload exists, because any payload is not shifted up
                fn remove_tcp_options<M: Sized + Send>(p: &mut Packet<TcpHeader, M>, h: &mut HeaderState) {
                    let old_offset = h.tcp.offset() as u16;
                    if old_offset > 20 {
                        debug!("trimming tcp-options by { } bytes", old_offset - 20);
                        h.tcp.set_data_offset(5u8);
                        // minimum mbuf data length is 60 bytes
                        h.ip.trim_length_by(old_offset - 20u16);
                        let trim_by = min(p.data_len() - 60usize, (old_offset - 20u16) as usize);
                        p.trim_payload_size(trim_by);
                        h.ip.update_checksum();
                    }
                }

                fn syn_received<M: Sized + Send>(
                    p: &mut Packet<TcpHeader, M>,
                    c: &mut Connection,
                    h: &mut HeaderState,
                    syn_counter: &usize,
                ) {
                    c.con_rec.push_s_state(TcpState::SynReceived);
                    c.dut_mac = h.mac.clone();
                    c.set_dut_sock(SocketAddrV4::new(Ipv4Addr::from(h.ip.src()), h.tcp.src_port()));
                    // debug!("checksum in = {:X}",p.get_header().checksum());
                    remove_tcp_options(p, h);
                    make_reply_packet(h);
                    //generate seq number:
                    c.s_seqn = 123456u32.wrapping_add((*syn_counter << 6) as u32);
                    h.tcp.set_seq_num(c.s_seqn);
                    update_tcp_checksum(p, h.ip.payload_size(0), h.ip.src(), h.ip.dst());
                    // debug!("checksum recalc = {:X}",p.get_header().checksum());
                    debug!("(SYN-)ACK to client, L3: { }, L4: { }", h.ip, h.tcp);
                }

                fn synack_received<M: Sized + Send>(p: &mut Packet<TcpHeader, M>, c: &mut Connection, h: &mut HeaderState) {
                    make_reply_packet(h);
                    c.c_ackn_nxt = h.tcp.ack_num();
                    h.tcp.unset_syn_flag();
                    h.tcp.set_seq_num(c.c_seqn_nxt);
                    update_tcp_checksum(p, h.ip.payload_size(0), h.ip.src(), h.ip.dst());
                }

                #[inline]
                fn generate_syn<M: Sized + Send>(
                    p: &mut Packet<TcpHeader, M>,
                    c: &mut Connection,
                    h: &mut HeaderState,
                    me: &L234Data,
                    servers: &Vec<L234Data>,
                    tx: &Sender<MessageFrom>,
                    pipeline_id: &PipelineId,
                    syn_counter: &mut usize,
                ) {
                    h.mac.set_etype(0x0800); // overwrite private ethertype tag
                    c.con_rec.server_index = *syn_counter as usize % servers.len();
                    set_header(&servers[c.con_rec.server_index], c.port(), h, me);
                    if *syn_counter == 0 {
                        tx.send(MessageFrom::GenTimeStamp(
                            pipeline_id.clone(),
                            "SYN",
                            *syn_counter,
                            utils::rdtsc_unsafe(),
                            0,
                        )).unwrap();
                    }

                    //generate seq number:
                    c.c_seqn_nxt = 123456u32.wrapping_add((*syn_counter << 12) as u32);
                    h.tcp.set_seq_num(c.c_seqn_nxt);
                    c.c_seqn_nxt = c.c_seqn_nxt.wrapping_add(1);
                    h.tcp.set_syn_flag();
                    h.tcp.set_window_size(5840); // 4* MSS(1460)
                    h.tcp.set_ack_num(0u32);
                    h.tcp.unset_ack_flag();
                    h.tcp.unset_psh_flag();

                    update_tcp_checksum(p, h.ip.payload_size(0), h.ip.src(), h.ip.dst());
                    *syn_counter += 1;
                    if *syn_counter % 1000 == 0 {
                        debug!("{}: sent {} SYNs", pipeline_id, *syn_counter);
                    }
                    //debug!("SYN packet to server - L3: {}, L4: {}", h.ip, p.get_header());
                }

                #[inline]
                fn make_payload_packet<M: Sized + Send>(
                    p: &mut Packet<TcpHeader, M>,
                    c: &mut Connection,
                    h: &mut HeaderState,
                    me: &L234Data,
                    servers: &Vec<L234Data>,
                    pipeline_id: &PipelineId,
                    payload: &Box<Vec<u8>>,
                ) {

                    h.mac.set_etype(0x0800); // overwrite private ethertype tag
                    set_header(&servers[c.con_rec.server_index], c.port(), h, me);
                    let sz= payload.len();
                    let ip_sz = h.ip.length();
                    p.add_to_payload_tail(sz).expect("insufficient tail room");
                    h.ip.set_length(ip_sz+sz as u16);
                    h.ip.update_checksum();
                    let tcp_length=h.ip.payload_size(0);
                    h.tcp.set_seq_num(c.c_seqn_nxt);
                    h.tcp.unset_syn_flag();
                    h.tcp.set_window_size(5840); // 4* MSS(1460)
                    h.tcp.set_ack_num(c.c_ackn_nxt);
                    h.tcp.set_ack_flag();
                    h.tcp.set_psh_flag();
                    c.c_seqn_nxt =c.c_seqn_nxt.wrapping_add(sz as u32);
                    p.copy_payload_from_bytearray(&payload);
                    if p.data_len() < MIN_FRAME_SIZE {
                        let n_padding_bytes = MIN_FRAME_SIZE - p.data_len();
                        debug!("padding with {} 0x0 bytes", n_padding_bytes);
                        p.add_padding(n_padding_bytes);
                    }
                    update_tcp_checksum(p, tcp_length, h.ip.src(), h.ip.dst());
                }

                let mut group_index = 0usize; // the index of the group to be returned, default 0: dump packet

                assert!(p.get_pre_header().is_some()); // we must have parsed the headers
                assert!(p.get_pre_pre_header().is_some()); // we must have parsed the headers

                // converting to raw pointer avoids to borrow mutably from p
                let mut hs = HeaderState {
                    ip: unsafe { &mut *(p.get_mut_pre_header().unwrap() as *mut IpHeader) },
                    mac: unsafe { &mut *(p.get_mut_pre_pre_header().unwrap() as *mut MacHeader) },
                    tcp: unsafe { &mut *(p.get_mut_header() as *mut TcpHeader) },
                };

                let hs_flow = hs.ip.flow().unwrap();
                // if set by the following tcp state machine,
                // the port/connection becomes released/ready afterwards
                // this is cumbersome, but we must make the  borrow checker happy
                let mut release_connection = None;
                let mut ready_connection = None;
                // check if we got a packet from generator

                match hs.mac.etype() {
                    PRIVATE_ETYPE_PACKET => {
                        if counter_to[TcpControls::SentSyn] < nr_connections {
                            let no_ready_connections= sm.ready_connections();
                            if let Some(c) = sm.create() {
                                generate_syn(
                                    p,
                                    c,
                                    &mut hs,
                                    &me,
                                    &servers,
                                    &tx_clone,
                                    &pipeline_id_clone,
                                    &mut counter_to[TcpControls::SentSyn],
                                );
                                c.con_rec.push_c_state(TcpState::SynSent);
                                wheel.schedule(
                                    &(timeouts.established.unwrap() * MILLIS_TO_CYCLES),
                                    c.port(),
                                );
                                if counter_to[TcpControls::SentSyn] == nr_connections {
                                    tx_clone
                                        .send(MessageFrom::GenTimeStamp(
                                            pipeline_id_clone.clone(),
                                            "SYN",
                                            counter_to[TcpControls::SentSyn],
                                            utils::rdtsc_unsafe(),
                                            0,
                                        )).unwrap();
                                    // we stop the injector, if nothing to do
                                    if no_ready_connections == 0 {
                                        injector_stop();
                                    }
                                }
                                group_index = 1;
                            }
                        } else {
                            let reply_socket=SocketAddrV4::new(Ipv4Addr::from(sm.ip()), sm.special_port());
                            if let Some(c) = sm.get_ready_connection() {
                                let payload=Box::new(serialize(&reply_socket).expect("cannot serialize SocketAddrV4"));
                                make_payload_packet(p, c, &mut hs, &me, &servers, &pipeline_id_clone, &payload );
                                trace!(
                                    "{}: sending payload packet with payload size {} on port {}",
                                    thread_id_2,
                                    payload.len(),
                                    c.port(),
                                );
                                group_index = 1;
                            }
                            else {  if injector_runs() { injector_stop(); } }
                        }
                    }
                    PRIVATE_ETYPE_TIMER => {
                        ticks += 1;
                        match rx.try_recv() {
                            Ok(MessageTo::FetchCounter) => {
                                tx_clone
                                    .send(MessageFrom::Counter(
                                        pipeline_id_clone.clone(),
                                        counter_to.clone(),
                                        counter_from.clone(),
                                    )).unwrap();
                            }
                            Ok(MessageTo::FetchCRecords) => {
                                sm.record_uncompleted();
                                tx_clone
                                    .send(MessageFrom::CRecords(pipeline_id_clone.clone(), sm.fetch_c_records()))
                                    .unwrap();
                            }
                            _ => {}
                        }
                        // check for timeouts
                        if ticks % wheel_tick_reduction_factor == 0 {
                            sm.release_timeouts(&utils::rdtsc_unsafe(), &mut wheel);
                        }
                    }
                    _ => {
                        if hs.tcp.dst_port() == sm.special_port() { //server side
                            let opt_c = if hs.tcp.syn_flag() {
                                debug!("got SYN on special port 0x{:x}", sm.special_port());
                                counter_from[TcpControls::RecvSyn] += 1;
                                sm.get_mut_or_insert(hs_flow.src_socket_addr(), &mut wheel)
                            } else {
                                sm.get_mut_by_sock(&hs_flow.src_socket_addr())
                            };
                            if opt_c.is_none() {
                                warn!("unexpected packet to server port or flow or out of resources");
                            } else {
                                let mut c = opt_c.unwrap();
                                let old_s_state = c.con_rec.last_s_state().clone();
                                let old_c_state = c.con_rec.last_c_state().clone();

                                if hs.tcp.syn_flag() {
                                    if old_s_state == TcpState::Closed {
                                        // replies with a SYN-ACK to client:
                                        syn_received(p, c, &mut hs, &mut counter_from[TcpControls::RecvSyn]);
                                        counter_from[TcpControls::SentSynAck] += 1;
                                        group_index = 1;
                                    } else {
                                        warn!("{} received SYN in state {:?}", thread_id_2, c.con_rec.s_states());
                                    }
                                } else if hs.tcp.ack_flag() && old_s_state == TcpState::SynReceived {
                                    c.s_con_established();
                                    counter_from[TcpControls::RecvSynAck2] += 1;
                                    debug!(
                                        "{} connection from DUT ({:?}) established",
                                        thread_id_2,
                                        hs_flow.src_socket_addr()
                                    );
                                } else if hs.tcp.fin_flag() {
                                    if old_s_state >= TcpState::FinWait1 {
                                        // we got a FIN as a receipt to a sent FIN (engine closed connection)
                                        debug!("received FIN-reply from DUT {:?}", hs_flow.src_socket_addr());
                                        counter_from[TcpControls::RecvFinAck] += 1;
                                        c.con_rec.push_s_state(TcpState::Closed);
                                    // TODO sent a final Ack
                                    } else {
                                        // DUT wants to close connection
                                        debug!(
                                            "{} DUT sends FIN on port {}/{} in state {:?}/{:?}",
                                            thread_id_2,
                                            hs.tcp.src_port(),
                                            c.port(),
                                            c.con_rec.last_c_state(),
                                            c.con_rec.last_s_state(),
                                        );
                                        c.con_rec.c_released(ReleaseCause::FinServer);
                                        counter_from[TcpControls::RecvFin] += 1;
                                        c.con_rec.push_s_state(TcpState::LastAck);
                                        make_reply_packet(&mut hs);
                                        hs.tcp.set_ack_flag();
                                        c.s_seqn = c.s_seqn.wrapping_add(1);
                                        hs.tcp.set_seq_num(c.s_seqn);
                                        update_tcp_checksum(p, hs.ip.payload_size(0), hs.ip.src(), hs.ip.dst());
                                        debug!("{} FIN-ACK to DUT, L3: { }, L4: { }", thread_id_2, hs.ip, hs.tcp);
                                        counter_from[TcpControls::SentFinAck] += 1;
                                        group_index = 1;
                                    }
                                } else if hs.tcp.rst_flag() {
                                    //c.con_rec.push_s_state(TcpState::Closed);
                                    counter_from[TcpControls::RecvRst] += 1;
                                    c.con_rec.push_s_state(TcpState::Closed);
                                    c.con_rec.c_released(ReleaseCause::RstServer);
                                    // release connection in the next block
                                    release_connection = Some(c.port());
                                } else if old_s_state == TcpState::LastAck && hs.tcp.ack_flag() {
                                    // received final ack from DUT for DUT initiated close
                                    //debug!("received final ACK for DUT initiated close on port { }", hs.tcp.dst_port());
                                    counter_from[TcpControls::RecvFinAck2] += 1;
                                    c.con_rec.push_s_state(TcpState::Closed);
                                    c.con_rec.c_released(ReleaseCause::FinServer);
                                    // release connection in the next block
                                    release_connection = Some(c.port());
                                }
                            }
                        } else { // client side
                            // check that flow steering worked:
                            assert!(sm.owns_tcp_port(hs.tcp.dst_port()));
                            let mut c = sm.get_mut_by_port(hs.tcp.dst_port());
                            if c.is_some() {
                                let mut c = c.as_mut().unwrap();
                                //debug!("incoming packet for connection {}", c);
                                let mut b_unexpected = false;
                                let old_c_state = c.con_rec.last_c_state().clone();

                                //check seqn
                                if old_c_state != TcpState::SynSent && hs.tcp.seq_num() != c.c_ackn_nxt {
                                    b_unexpected = true;
                                    warn!("{} unexpected sequence number in state {:?}, tcp-header {}", thread_id_2, old_c_state, hs.tcp);
                                }
                                else if hs.tcp.ack_flag() && hs.tcp.syn_flag() {
                                    group_index = 1;
                                    counter_to[TcpControls::RecvSynAck] += 1;
                                    if old_c_state == TcpState::SynSent {
                                        c.c_con_established();
                                        ready_connection=Some(c.port());
                                        //tx_clone
                                        //    .send(MessageFrom::Established(pipeline_id_clone.clone(), c.con_rec.clone()))
                                        //    .unwrap();
                                        //debug!(
                                        //    "established two-way client server connection, SYN-ACK received: L3: {}, L4: {}",
                                        //    hs.ip, hs.tcp
                                        //);
                                        synack_received(p, &mut c, &mut hs);
                                        counter_to[TcpControls::SentAck] += 1;
                                    } else if old_c_state == TcpState::Established {
                                        synack_received(p, &mut c, &mut hs);
                                        counter_to[TcpControls::SentAck] += 1;
                                    } else {
                                        warn!("{} received SYN-ACK in wrong state: {:?}",thread_id_2, old_c_state);
                                        group_index = 0;
                                    } // ignore the SynAck
                                } else if hs.tcp.fin_flag() {
                                    if old_c_state >= TcpState::FinWait1 {
                                        counter_to[TcpControls::RecvFinAck] += 1;
                                        // got FIN receipt to a client initiated FIN
                                        debug!("received FIN-reply from server on port {}", hs.tcp.dst_port());
                                        c.con_rec.push_c_state(TcpState::Closed);
                                    // TODO return an ACK
                                    } else {
                                        // server initiated TCP close
                                        /*
                                    debug!(
                                        "server closes connection on port {} in client state {:?}",
                                        c.p_port(),
                                        c.con_rec.c_states(),
                                    );
                                    */
                                        //c.con_rec.push_s_state(TcpState::FinWait1);
                                        counter_to[TcpControls::RecvFin] += 1;
                                        c.con_rec.push_c_state(TcpState::LastAck);
                                        make_reply_packet(&mut hs);
                                        hs.tcp.set_ack_flag();
                                        c.c_ackn_nxt=hs.tcp.ack_num();
                                        hs.tcp.set_seq_num(c.c_seqn_nxt);
                                        counter_to[TcpControls::SentFinAck] += 1;
                                        update_tcp_checksum(p, hs.ip.payload_size(0), hs.ip.src(), hs.ip.dst());
                                        group_index = 1;
                                    }
                                } else if hs.tcp.rst_flag() {
                                    //c.con_rec.push_s_state(TcpState::Closed);
                                    counter_to[TcpControls::RecvRst] += 1;
                                    c.con_rec.push_c_state(TcpState::Closed);
                                    c.con_rec.c_released(ReleaseCause::RstServer);
                                    // release connection in the next block
                                    release_connection = Some(c.port());
                                } else if old_c_state == TcpState::LastAck && hs.tcp.ack_flag() {
                                    // received final ack from server for server initiated close
                                    //debug!("received final ACK for server initiated close on port { }", hs.tcp.dst_port());
                                    //c.con_rec.push_s_state(TcpState::Closed);
                                    counter_to[TcpControls::RecvFinAck2] += 1;
                                    c.con_rec.push_c_state(TcpState::Closed);
                                    c.con_rec.c_released(ReleaseCause::FinServer);
                                    // release connection in the next block
                                    release_connection = Some(c.port());
                                } else if hs.tcp.ack_flag() {


                                } else {
                                    counter_to[TcpControls::Unexpected] += 1;
                                    // debug!("received from server { } in c/s state {:?}/{:?} ", hs.tcp, c.con_rec.c_state, c.con_rec.s_state);
                                    b_unexpected = true; //  except we revise it, see below
                                }

                                if b_unexpected {
                                    warn!(
                                        "{} unexpected TCP packet on port {} in client state {:?}, sending to KNI i/f: {}",
                                        thread_id_2,
                                        hs.tcp.dst_port(),
                                        c.con_rec.c_states(),
                                        hs.tcp,
                                    );
                                    group_index = 2;
                                }
                            } else {
                                warn!("{} engine has no state for {}:{}, sending to KNI i/f",
                                      thread_id_2,
                                      hs.ip,
                                      hs.tcp,
                                      //Ipv4Addr::from(hs.ip.dst()),
                                      // hs.tcp.dst_port(),
                                );
                                // we send this to KNI which handles out-of-order TCP, e.g. by sending RST
                                group_index = 2;
                            }
                        }
                    }
                }

                // here we check if we shall release the connection state,
                // need this cumbersome way because of borrow checker for the state manager sm
                if let Some(sport) = release_connection {
                    //debug!("releasing port {}", sport);
                    sm.release_port(sport);
                }
                if let Some(sport) = ready_connection {
                    trace!("{} connection on  port {} is ready", thread_id_2, sport);
                    sm.set_ready_connection(sport);
                    // if this is the first ready connection, we restart the injector, avoid accessing Atomic too often
                    if sm.ready_connections()==1 { injector_start(); }
                }
                do_ttl(&mut hs);
                group_index
            },
            sched,
            uuid_l4groupby_clone,
        );

    let l2kniflow = l2groups.get_group(0).unwrap().compose();
    let l4kniflow = l4groups.get_group(2).unwrap().compose();
    let pipe2kni = merge(vec![l2kniflow, l4kniflow]).send(kni.clone());
    let l4pciflow = l4groups.get_group(1).unwrap().compose();
    let l4dumpflow = l4groups.get_group(0).unwrap().filter(box move |_| false).compose();
    let pipe2pci = merge(vec![l4pciflow, l4dumpflow]).send(pci.clone());
    /*
    let uuid_pipe2kni = Uuid::new_v4();
    let name = String::from("Pipe2Kni");
    sched.add_runnable(Runnable::from_task(uuid_pipe2kni, name, pipe2kni).unready());
    tx.send(MessageFrom::Task(pipeline_id.clone(), uuid_pipe2kni, TaskType::Pipe2Kni))
        .unwrap();
    */
    install_task(sched, &tx, &pipeline_id, "Pipe2Kni", TaskType::Pipe2Kni, pipe2kni);

    /*
    let uuid_pipe2pci = Uuid::new_v4();
    let name = String::from("Pipe2Pci");
    sched.add_runnable(Runnable::from_task(uuid_pipe2pci, name, pipe2pci).unready());
    tx.send(MessageFrom::Task(pipeline_id.clone(), uuid_pipe2pci, TaskType::Pipe2Pci))
        .unwrap();
*/
    install_task(sched, &tx, &pipeline_id, "Pipe2Pci", TaskType::Pipe2Pci, pipe2pci);
}
