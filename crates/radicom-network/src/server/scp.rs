//! The DIMSE SCP: association handshake, per-connection Serving, dispatch.

use super::options::ConnectionCountGuard;
use super::wire::*;
use super::*;

/// A DIMSE server bound to one address (§11.4).
pub struct DicomServer<P: ServiceProvider> {
    listener: TcpListener,
    provider: Arc<P>,
    options: ServerOptions,
    local_addr: std::net::SocketAddr,
    _registration: crate::registry::DicomServerRegistration,
    connected_clients: Arc<AtomicUsize>,
}

impl<P: ServiceProvider + 'static> DicomServer<P> {
    /// Binds and starts accepting (§11.4).
    pub async fn bind(
        addr: impl tokio::net::ToSocketAddrs,
        provider: Arc<P>,
        mut options: ServerOptions,
    ) -> Result<Self> {
        for syntax in &mut options.supported_transfer_syntaxes {
            let known = radicom::model::transfer_syntax::query(syntax).ok_or_else(|| {
                NetworkError::protocol("server policy contains an unknown transfer syntax")
            })?;
            *syntax = known.uid().to_string();
        }
        let listener = TcpListener::bind(addr).await.map_err(NetworkError::Io)?;
        let local_addr = listener.local_addr().map_err(NetworkError::Io)?;
        let registration = crate::registry::DicomServerRegistry::register(local_addr)?;
        Ok(DicomServer {
            listener,
            provider,
            options,
            local_addr,
            _registration: registration,
            connected_clients: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The bound address, useful when binding to port 0.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// Returns a handle that observes live connection tasks after `run` moves
    /// the server into its accept loop.
    pub fn connected_clients(&self) -> ConnectedClients {
        ConnectedClients {
            count: self.connected_clients.clone(),
        }
    }

    /// Serves connections until the listener errors (§11.4).
    pub async fn run(self) -> Result<()> {
        let (_shutdown, receiver) = tokio::sync::watch::channel(false);
        self.run_until(receiver).await
    }

    /// Serves connections until the listener errors or `shutdown` is set.
    /// Existing connection tasks are allowed to finish when shutdown starts.
    ///
    /// Each connection gets its own task; `max_clients` bounds concurrency by
    /// gating the accept loop, so a full server leaves further peers in the
    /// listen backlog instead of spawning unbounded tasks.
    pub async fn run_until(self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let provider = self.provider.clone();
        let options = self.options.clone();
        let connected_clients = self.connected_clients.clone();
        // 0 = unlimited (a semaphore with no permits would block forever)
        let slots = (options.max_clients > 0)
            .then(|| Arc::new(tokio::sync::Semaphore::new(options.max_clients)));
        loop {
            // B: take a client slot *before* accepting, so a full server leaves
            // the next peer in the listen backlog instead of accepting it and
            // then blocking (with no timeout) on the semaphore.
            let permit = match &slots {
                Some(slots) => {
                    let permit = tokio::select! {
                        changed = shutdown.changed() => {
                            match changed {
                                Ok(()) if *shutdown.borrow() => return Ok(()),
                                Ok(()) => continue,
                                Err(_) => return Ok(()),
                            }
                        }
                        permit = slots.clone().acquire_owned() => permit,
                    };
                    Some(permit.map_err(|_| NetworkError::protocol("connection limiter closed"))?)
                }
                None => None,
            };
            let accepted = tokio::select! {
                changed = shutdown.changed() => {
                    match changed {
                        Ok(()) if *shutdown.borrow() => return Ok(()),
                        // dropping the permit releases the reserved slot
                        Ok(()) => continue,
                        Err(_) => return Ok(()),
                    }
                }
                accepted = self.listener.accept() => accepted,
            };
            let (stream, _peer) = match accepted {
                Ok(pair) => pair,
                // the permit is dropped here, freeing the slot, then we return
                Err(e) => return Err(NetworkError::Io(e)),
            };
            // advisory: a refused buffer size must not drop the connection
            let _ = crate::transport::apply_options(&stream, &options.tcp);
            let provider = provider.clone();
            let options = options.clone();
            connected_clients.fetch_add(1, Ordering::AcqRel);
            let count_guard = ConnectionCountGuard {
                count: connected_clients.clone(),
            };
            tokio::spawn(async move {
                // the slot is released when the connection task ends
                let _permit = permit;
                let _count_guard = count_guard;
                #[cfg(feature = "tls")]
                if let Some(tls) = options.tls.clone() {
                    // The handshake runs while this task already holds a client
                    // slot, so it needs its own deadline: a peer that connects
                    // and never sends a ClientHello must not hold the slot
                    // forever (review #14).
                    // A failed or timed-out handshake ends this connection
                    // only; the permit is released when the task returns.
                    if let Ok(Ok(stream)) =
                        tokio::time::timeout(options.tls_handshake_timeout, tls.accept(stream))
                            .await
                    {
                        if let Err(error) = serve_connection(stream, provider, options).await {
                            tracing::debug!(target: "radicom::network", %error, "connection ended with an error");
                        }
                    }
                    return;
                }
                if let Err(error) = serve_connection(stream, provider, options).await {
                    tracing::debug!(target: "radicom::network", %error, "connection ended with an error");
                }
            });
        }
    }

    /// Serves exactly one connection, then returns (test and single-shot use).
    pub async fn accept_one(&self) -> Result<()> {
        let (stream, _peer) = self.listener.accept().await.map_err(NetworkError::Io)?;
        crate::transport::apply_options(&stream, &self.options.tcp)?;
        self.connected_clients.fetch_add(1, Ordering::AcqRel);
        let _count_guard = ConnectionCountGuard {
            count: self.connected_clients.clone(),
        };
        #[cfg(feature = "tls")]
        if let Some(tls) = self.options.tls.clone() {
            let stream =
                tokio::time::timeout(self.options.tls_handshake_timeout, tls.accept(stream))
                    .await
                    .map_err(|_| NetworkError::TlsHandshakeTimedOut {
                        timeout_ms: self.options.tls_handshake_timeout.as_millis() as u64,
                    })??;
            return serve_connection(stream, self.provider.clone(), self.options.clone()).await;
        }
        serve_connection(stream, self.provider.clone(), self.options.clone()).await
    }
}

/// Drives one accepted connection through association, one exchange, release (§11.1).
pub async fn serve_connection<P: ServiceProvider + 'static, S>(
    stream: S,
    provider: Arc<P>,
    options: ServerOptions,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // -- observability (OBS-01): count this connection's bytes; the guard
    // reports open/close so every return path is covered ------------------
    let metrics = options.metrics.clone();
    let _connection = crate::observability::ConnectionGuard::new(metrics.clone());
    let mut stream = crate::observability::CountingStream::new(stream, metrics.clone());

    let result = serve_connection_inner(&mut stream, provider, options).await;
    // a violation the dispatcher already answered with its own A-ABORT must not
    // be aborted twice: `PeerViolation` is the typed signal for that (NET-26)
    if matches!(result, Err(NetworkError::Protocol(_))) {
        let abort = Abort {
            source: AbortSource::Provider,
            reason: 0,
        };
        let _ = write_pdu(&mut stream, &Pdu::Abort(abort)).await;
    }
    result
}

pub async fn serve_connection_inner<P: ServiceProvider + 'static, S>(
    mut stream: &mut crate::observability::CountingStream<S>,
    provider: Arc<P>,
    options: ServerOptions,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // -- association handshake ------------------------------------------
    // The ACSE phase has its own timeout: a peer that connects and never sends
    // (or half-sends) an A-ASSOCIATE-RQ must not hold this task and fd forever.
    let request =
        match tokio::time::timeout(options.association_timeout, read_pdu(&mut stream)).await {
            Ok(Ok(Some(Pdu::AssociateRq(request)))) => request,
            Ok(Ok(_)) => return Err(NetworkError::protocol("expected A-ASSOCIATE-RQ")),
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                // PS3.8 §9.3.8: provider-initiated abort, reason "not specified"
                let abort = Abort {
                    source: AbortSource::Provider,
                    reason: 0,
                };
                let _ = write_pdu(&mut stream, &Pdu::Abort(abort)).await;
                return Err(NetworkError::AssociationRequestTimedOut { attempts: 1 });
            }
        };

    if let Some(expected) = options.called_ae_title.as_deref() {
        if request.called_ae != expected {
            let reject = AssociateRj {
                result: RejectResult::Permanent,
                source: RejectSource::User,
                reason: 7,
            };
            write_pdu(&mut stream, &Pdu::AssociateRj(reject)).await?;
            return Ok(());
        }
    }

    // PS3.8 §9.3.2: the RQ names the protocol version and the application
    // context; anything else is a rejection, not something to negotiate around.
    if request.protocol_version != 1 {
        let reject = AssociateRj {
            result: RejectResult::Permanent,
            source: RejectSource::ProviderAcse,
            reason: 2, // protocol version not supported
        };
        write_pdu(&mut stream, &Pdu::AssociateRj(reject)).await?;
        return Ok(());
    }
    if request.application_context != APPLICATION_CONTEXT {
        let reject = AssociateRj {
            result: RejectResult::Permanent,
            source: RejectSource::User,
            reason: 2, // application context name not supported
        };
        write_pdu(&mut stream, &Pdu::AssociateRj(reject)).await?;
        return Ok(());
    }
    // a malformed context set is a broken RQ: the connection wrapper answers
    // with an A-ABORT, since no rejection reason describes it (§9.3.2/§9.3.8)
    validate_proposed_contexts(&request.presentation_contexts)?;

    let mut policy = NegotiationPolicy::new(provider.supported_sop_classes());
    policy.supported_syntaxes = if options.supported_transfer_syntaxes.is_empty() {
        vec![
            syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid().into(),
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid().into(),
        ]
    } else {
        options.supported_transfer_syntaxes.clone()
    };
    policy.priority = options.syntax_priority;
    // this SCP also acts as a Storage SCU during C-GET (reverse C-STORE), so it
    // grants the roles proposed for the classes it supports (review #16)
    policy.accept_roles = true;
    let mut negotiated = policy.negotiate(&request.presentation_contexts);
    policy.negotiate_roles(&mut negotiated, &request.user_information.role_selections);
    // reject when nothing was accepted (PS3.8 Table 9-21, source 1, reason 1:
    // "no reason given" — there is no dedicated "no acceptable context" code)
    if !negotiated.iter().any(|c| c.is_accepted()) {
        let reject = AssociateRj {
            result: RejectResult::Permanent,
            source: RejectSource::User,
            reason: 1,
        };
        write_pdu(&mut stream, &Pdu::AssociateRj(reject)).await?;
        return Ok(());
    }

    // the AC repeats the RQ's AE titles in the RQ's order: called first (§9.3.3)
    let mut accept = Associate::new(request.called_ae.clone(), request.calling_ae.clone());
    accept.presentation_contexts = negotiated.iter().map(|c| c.to_wire()).collect();
    let mut user = crate::association::build_user_information(
        options.max_pdu_length,
        IMPLEMENTATION_CLASS_UID,
        IMPLEMENTATION_VERSION_NAME,
        // the AC echoes exactly the roles granted above (§11.2, review #16)
        negotiated.iter().flat_map(|c| c.roles.clone()).collect(),
    );

    // Bound both the negotiated queue and an unlimited configuration to 64.
    let capacity = crate::pdu::AsyncOperations {
        invoked: match options.async_operations.invoked {
            0 => 64,
            n => n.min(64),
        },
        performed: match options.async_operations.performed {
            0 => 64,
            n => n.min(64),
        },
    };
    user.async_operations = request
        .user_information
        .async_operations
        .map(|offered| capacity.negotiate_as_acceptor(offered));
    let incoming_window = usize::from(user.async_operations.unwrap_or_default().invoked);

    // extended negotiation: the AC echoes one item per requested SOP class (§11.2)
    user.extended_negotiations = options
        .extended_negotiation
        .answer(&request.user_information.extended_negotiations);

    // user identity: refused identities reject the association (§11.2)
    if let Some(identity) = &request.user_information.user_identity {
        let Some(response) = options.user_identity.answer(identity) else {
            let reject = AssociateRj {
                result: RejectResult::Permanent,
                source: RejectSource::User,
                reason: 1, // no reason given
            };
            write_pdu(&mut stream, &Pdu::AssociateRj(reject)).await?;
            return Ok(());
        };
        user.user_identity_response = response;
    }

    accept.user_information = user;
    write_pdu(&mut stream, &Pdu::AssociateAc(accept)).await?;
    // the association is now established (OBS-01): report open/close
    let _association = crate::observability::AssociationGuard::new(options.metrics.clone());

    let accepted = AcceptedContexts::from_negotiated(negotiated);
    // The Max-PDU we send must fit what the peer can receive, but a hostile
    // peer can advertise 0xFFFF_FFFF. Clamp it to our own ceiling so the send
    // path (PduFramer) never pre-allocates a multi-GiB buffer (PS3.8 §9.3.2).
    let peer_max_pdu = negotiated_send_max_pdu(
        request.user_information.max_pdu_length,
        options.max_pdu_length,
    );
    let splitter = PduSplitter::with_limits(
        peer_max_pdu,
        options.max_pdvs_per_pdu,
        options.max_command_buffer.map(|limit| limit as usize),
        options.max_data_buffer.map(|limit| limit as usize),
    );
    // a command awaiting its dataset (§11.2)
    let mut awaiting_dataset: std::collections::HashMap<u8, Command> =
        std::collections::HashMap::new();

    let mut serving = Serving {
        stream: &mut stream,
        splitter: &splitter,
        provider: &provider,
        options: &options,
        // Bound non-storage (query/move/get/N) datasets in memory; C-STORE
        // datasets are armed and streamed to a spool instead.
        assembler: PdvAssembler::with_spool_and_limits(
            options.store_spool.clone(),
            PdvAssembler::DEFAULT_MAX_NON_STORAGE_MESSAGE,
        ),
        pending_units: VecDeque::new(),
        deferred_units: VecDeque::new(),
        deferred_requests: 0,
        incoming_window,
        accepted: &accepted,
        reverse_message_id: 1,
        transcoder_cache: std::collections::HashMap::new(),
        // E: accept exactly the Maximum-length-received this side advertised
        pdu_reader: crate::transport::PduReader::with_max_pdu(crate::pdu::clamp_max_pdu_length(
            options.max_pdu_length,
        )),
    };

    // -- message loop ----------------------------------------------------
    //
    // M6: a request that needs no further read from this association runs on
    // its own task (see [`DispatchJob`]). This loop is still the only writer,
    // so one response is written in one batch and the PDVs of two messages
    // never interleave (§11.1); the message ID in the response is the request's,
    // so finishing order does not matter. `select!` prefers a ready response
    // over the next read, and a dropped read keeps its half frame in the
    // cancel-safe framer (review #13).
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Result<Outgoing>>(64);
    let mut jobs: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    let mut active_request_ids = std::collections::HashSet::new();
    // a C-CANCEL for a request running on a task is forwarded through here
    let cancels: CancelMap = CancelMap::default();
    let max_jobs = if incoming_window == 0 {
        MAX_CONCURRENT_REQUESTS
    } else {
        incoming_window.clamp(1, MAX_CONCURRENT_REQUESTS)
    };
    loop {
        tokio::select! {
            biased;
            outgoing = out_rx.recv() => {
                if let Some(outgoing) = outgoing {
                    let outgoing = outgoing?;
                    serving
                        .send_with_dataset(
                            outgoing.context_id,
                            &outgoing.command,
                            outgoing.dataset.as_ref(),
                            &outgoing.syntax,
                        )
                        .await?;
                    if let Some(id) = outgoing.command.message_id_being_responded_to {
                        active_request_ids.remove(&id);
                    }
                }
            }
            joined = jobs.join_next(), if !jobs.is_empty() => {
                if let Some(Err(error)) = joined {
                    // a panicking provider leaves a request unanswered: the
                    // association cannot be trusted to stay in step
                    tracing::error!("a dispatched request task failed: {error}");
                    return Err(NetworkError::protocol("a dispatched request task failed"));
                }
            }
            maybe = serving.next_pdu() => {
        let Some(pdu) = maybe? else {
            return Ok(()); // peer closed
        };
        match pdu {
            Pdu::PData(pdata) => {
                let units = serving.push_pdata_offloaded(&pdata).await?;
                serving.pending_units.extend(units);
                while let Some(unit) = serving.pending_units.pop_front() {
                    match unit.stream {
                        Stream::Command => {
                            // PS3.8 §9.3.3: only an accepted context may carry
                            // messages; anything else is a protocol violation.
                            if serving.accepted.by_id(unit.context_id).is_none() {
                                return Err(NetworkError::protocol(
                                    "command on an unaccepted presentation context",
                                ));
                            }
                            let command = match command_from_bytes(&unit.bytes) {
                                Ok(command) => command,
                                Err(_error) => {
                                    // A malformed command is a service-user
                                    // violation; tell the peer before closing.
                                    let abort = Abort {
                                        source: AbortSource::User,
                                        reason: 0,
                                    };
                                    serving.send_pdu(&Pdu::Abort(abort)).await?;
                                    return Err(NetworkError::PeerViolation(
                                        "peer sent a malformed command",
                                    ));
                                }
                            };
                            serving
                                .accepted
                                .validate_request(unit.context_id, &command, true)?;
                            // Only C-STORE, C-FIND, C-MOVE and C-GET carry a
                            // dataset (§7.2..§11.4). Any other request is
                            // answered at once: trusting the peer's
                            // (0000,0800) flag would hang the engine on a peer
                            // that sets it wrongly.
                            let expects_dataset = !command.no_dataset
                                && matches!(
                                    command.field,
                                    CommandField::CStoreRq
                                        | CommandField::CFindRq
                                        | CommandField::CMoveRq
                                        | CommandField::CGetRq
                                        | CommandField::NEventReportRq
                                        | CommandField::NCreateRq
                                        | CommandField::NSetRq
                                        | CommandField::NActionRq
                                );
                            if expects_dataset {
                                // H3: a C-STORE dataset streams to a temp file
                                // instead of growing in memory. Armed before its
                                // first data PDV can arrive.
                                if command.field == CommandField::CStoreRq {
                                    serving.assembler.arm_spill(unit.context_id);
                                }
                                // the dataset arrives in later PDVs; keyed by
                                // context so a second request cannot displace it
                                if awaiting_dataset.insert(unit.context_id, command).is_some() {
                                    return Err(NetworkError::protocol(
                                        "overlapping command/dataset pairs on one context",
                                    ));
                                }
                            } else if command.field == CommandField::CCancelRq {
                                // SRV-08: a request running on a task cannot read
                                // this socket, so the loop forwards the cancel
                                if let Some(id) = command.message_id_being_responded_to {
                                    if let Some(cancel) = cancels.lock().unwrap().get(&id) {
                                        let _ = cancel.send(true);
                                    }
                                }
                                handle_command(&mut serving, unit.context_id, command).await?;
                                serving.resume_deferred();
                            } else if DispatchJob::is_concurrent(&command) {
                                if !active_request_ids.insert(command.message_id) {
                                    return Err(NetworkError::protocol(
                                        "duplicate outstanding DIMSE Message ID",
                                    ));
                                }
                                // C: keep concurrent providers within the
                                // negotiated window. When it is full, wait for a
                                // running job to finish (writing its response)
                                // instead of running this provider inline, which
                                // let max_jobs + 1 run at once.
                                while jobs.len() >= max_jobs {
                                    tokio::select! {
                                        outgoing = out_rx.recv() => {
                                            if let Some(outgoing) = outgoing {
                                                let outgoing = outgoing?;
                                                serving
                                                    .send_with_dataset(
                                                        outgoing.context_id,
                                                        &outgoing.command,
                                                        outgoing.dataset.as_ref(),
                                                        &outgoing.syntax,
                                                    )
                                                    .await?;
                                                if let Some(id) = outgoing.command.message_id_being_responded_to {
                                                    active_request_ids.remove(&id);
                                                }
                                            }
                                        }
                                        joined = jobs.join_next() => {
                                            if let Some(Err(error)) = joined {
                                                tracing::error!(
                                                    "a dispatched request task failed: {error}"
                                                );
                                                return Err(NetworkError::protocol(
                                                    "a dispatched request task failed",
                                                ));
                                            }
                                        }
                                    }
                                }
                                // M6: the provider runs on its own task while this
                                // loop keeps reading the association
                                let syntax = match serving.accepted.by_id(unit.context_id) {
                                    Some(context) => {
                                        transfer_syntax_of(&context.transfer_syntax)?
                                    }
                                    None => {
                                        return Err(NetworkError::protocol(
                                            "command on an unaccepted presentation context",
                                        ))
                                    }
                                };
                                jobs.spawn(run_job(
                                    DispatchJob {
                                        context_id: unit.context_id,
                                        command,
                                        body: MessageBody::Memory(bytes::Bytes::new()),
                                        syntax,
                                        cancel: None,
                                    },
                                    provider.clone(),
                                    options.clone(),
                                    out_tx.clone(),
                                ));
                                serving.resume_deferred();
                            } else {
                                handle_command(&mut serving, unit.context_id, command).await?;
                                serving.resume_deferred();
                            }
                        }
                        Stream::Data => {
                            let Some(command) = awaiting_dataset.remove(&unit.context_id) else {
                                return Err(NetworkError::protocol(
                                    "dataset PDV without a command",
                                ));
                            };
                            let context_id = unit.context_id;
                            // the PDV names its context: use that context's
                            // accepted syntax, never another context of the
                            // same SOP class (review #15)
                            let context = accepted.by_id(context_id).ok_or_else(|| {
                                NetworkError::protocol(
                                    "dataset on an unaccepted presentation context",
                                )
                            })?;
                            let syntax = transfer_syntax_of(&context.transfer_syntax)?;
                            if DispatchJob::is_concurrent(&command) {
                                // C: wait for a free window slot rather than
                                // running this provider inline (max_jobs + 1).
                                while jobs.len() >= max_jobs {
                                    tokio::select! {
                                        outgoing = out_rx.recv() => {
                                            if let Some(outgoing) = outgoing {
                                                let outgoing = outgoing?;
                                                serving
                                                    .send_with_dataset(
                                                        outgoing.context_id,
                                                        &outgoing.command,
                                                        outgoing.dataset.as_ref(),
                                                        &outgoing.syntax,
                                                    )
                                                    .await?;
                                            }
                                        }
                                        joined = jobs.join_next() => {
                                            if let Some(Err(error)) = joined {
                                                tracing::error!(
                                                    "a dispatched request task failed: {error}"
                                                );
                                                return Err(NetworkError::protocol(
                                                    "a dispatched request task failed",
                                                ));
                                            }
                                        }
                                    }
                                }
                                let cancel = (command.field == CommandField::CFindRq)
                                    .then(|| CancelSlot::register(&cancels, command.message_id));
                                jobs.spawn(run_job(
                                    DispatchJob {
                                        context_id,
                                        command,
                                        body: unit.body(),
                                        syntax: syntax.clone(),
                                        cancel,
                                    },
                                    provider.clone(),
                                    options.clone(),
                                    out_tx.clone(),
                                ));
                            } else {
                                handle_command_with_dataset(
                                    &mut serving,
                                    context_id,
                                    command,
                                    unit.body(),
                                    &syntax,
                                )
                                .await?;
                            }
                            serving.resume_deferred();
                        }
                    }
                }
            }
            Pdu::ReleaseRq => {
                serving.send_pdu(&Pdu::ReleaseRp).await?;
                if !jobs.is_empty() {
                    // PS3.8 §9.3: the requestor releases once its operations
                    // completed; releasing with one still running ends the
                    // association abnormally, as it did before M6
                    return Err(NetworkError::ClosedPrematurely { source: None });
                }
                return Ok(());
            }
            // PS3.8 §9.3.8: the peer gave up on the association. Returning Ok
            // here would report a peer abort as a clean close.
            Pdu::Abort(abort) => {
                return Err(NetworkError::Abort {
                    source_code: abort.source.code(),
                    reason: abort.reason,
                })
            }
            other => {
                return Err(NetworkError::protocol(match other {
                    Pdu::AssociateRq(_) => "unexpected A-ASSOCIATE-RQ after association",
                    Pdu::AssociateAc(_) => "unexpected A-ASSOCIATE-AC",
                    Pdu::AssociateRj(_) => "unexpected A-ASSOCIATE-RJ",
                    Pdu::ReleaseRp => "unexpected A-RELEASE-RP",
                    _ => "unexpected PDU",
                }))
            }
        }
            }
        }
    }
}

/// Connects, failing with `ConnectTimedOut` when `timeout` expires first (M7).
pub async fn connect_with_timeout<F>(connect: F, timeout: Option<Duration>) -> Result<TcpStream>
where
    F: std::future::Future<Output = std::io::Result<TcpStream>>,
{
    match timeout {
        None => connect.await.map_err(NetworkError::Io),
        Some(limit) => tokio::time::timeout(limit, connect)
            .await
            .map_err(|_| NetworkError::ConnectTimedOut {
                timeout_ms: limit.as_millis() as u64,
            })?
            .map_err(NetworkError::Io),
    }
}

/// The C-GET path: deliver instances back as reverse C-STORE sub-operations
/// (§11.4).
pub async fn handle_c_get<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    dataset_bytes: &[u8],
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let identifier = match crate::message::decode_dataset_with_fallback(
        dataset_bytes,
        syntax,
        serving.options.fallback_encoding,
    ) {
        Ok(identifier) => identifier,
        Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
    };

    let request = crate::cget::CGetRequest {
        message_id: command.message_id,
        sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
        priority: command.priority,
        identifier,
    };

    use futures::StreamExt;
    let provider = serving.provider.clone();
    let mut stream = provider.on_c_get(&request);
    let mut summary = crate::cmove::SubOperationSummary::default();
    let mut provider_summary = None;
    let mut saw_instance = false;
    let mut failed_uids = Vec::new();
    // A provider that reports its own progress owns (0000,1020): forwarding its
    // numbers is the only way to preserve the real remaining count while the
    // stream is consumed incrementally.
    let mut remaining = 0u16;
    let mut terminated = false;
    loop {
        if let Some(unit) = serving.pending_units.pop_front() {
            if unit.stream != Stream::Command {
                serving.defer(unit)?;
                continue;
            }
            let cancel = command_from_bytes(&unit.bytes)?;
            if cancel.field == CommandField::CCancelRq {
                if cancel.message_id_being_responded_to == Some(request.message_id)
                    && unit.context_id == context_id
                {
                    let mut response = crate::cget::CGetResponse::failure(
                        &request,
                        crate::dimse::status::CANCEL,
                        "C-GET cancelled",
                    );
                    response.remaining = remaining;
                    response.summary = if saw_instance {
                        summary
                    } else {
                        provider_summary.unwrap_or(summary)
                    };
                    response.failed_sop_instance_uids = failed_uids.clone();
                    serving
                        .send_with_dataset(
                            context_id,
                            &response.to_command(),
                            response.identifier().as_ref(),
                            syntax,
                        )
                        .await?;
                    return Ok(());
                }
                if serving.incoming_window > 1 {
                    serving.defer(unit)?;
                }
                continue;
            }
            serving.defer(unit)?;
            continue;
        }

        tokio::select! {
            biased;
            pdu = serving.next_pdu() => {
                let Some(pdu) = pdu? else { return Ok(()) };
                match pdu {
                    Pdu::PData(pdata) => {
                        let units = serving.push_pdata_offloaded(&pdata).await?;
                        serving.pending_units.extend(units);
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason });
                    }
                    Pdu::ReleaseRq => {
                        serving.send_pdu(&Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => return Err(NetworkError::protocol("unexpected PDU while C-GET is in flight")),
                }
            }
            event = stream.next() => {
                let Some(event) = event else { break };
                let event = match event {
                    Ok(event) => event,
                    Err(error) => crate::cget::CGetEvent::Response(crate::cget::CGetResponse::failure(
                        &request,
                        crate::dimse::status::PROCESSING_FAILURE,
                        error.to_string(),
                    )),
                };
                match event {
                    crate::cget::CGetEvent::Instance(file) => {
                        let (state, cancelled) = match serving
                            .send_reverse_store(&file, context_id, request.message_id)
                            .await
                        {
                            Ok(result) => result,
                            // PS3.4 C.4.2: an instance that cannot be delivered is one
                            // failed sub-operation, not a reason to drop the
                            // association and every instance already sent
                            Err(error) if is_fatal(&error) => return Err(error),
                            Err(_) => (StatusState::Failure, false),
                        };
                        if state == StatusState::Failure {
                            if let Some(uid) = file.dataset.try_get_str(Tag::SOP_INSTANCE_UID) { failed_uids.push(uid.trim().to_string()); }
                        }
                        summary.record(state);
                        saw_instance = true;
                        if cancelled {
                            let mut response = crate::cget::CGetResponse::failure(
                                &request,
                                crate::dimse::status::CANCEL,
                                "C-GET cancelled",
                            );
                            response.remaining = remaining;
                            response.summary = summary;
                            response.failed_sop_instance_uids = failed_uids.clone();
                            serving.send_with_dataset(context_id, &response.to_command(), response.identifier().as_ref(), syntax).await?;
                            return Ok(());
                        }
                    }
                    crate::cget::CGetEvent::Response(response) => {
                        if response.is_pending() {
                            remaining = response.remaining;
                            provider_summary = Some(response.summary);
                            let mut response = response;
                            if saw_instance {
                                response.summary = summary;
                            }
                            serving.send_with_dataset(context_id, &response.to_command(), response.identifier().as_ref(), syntax).await?;
                        } else {
                            terminated = true;
                            // Preserve provider failures/warnings even when they have
                            // no sub-operation counters. Once instances have been
                            // handed to the engine, its reverse-store outcomes are
                            // authoritative for the terminal counters.
                            let mut terminal = if saw_instance {
                                if response.state() == crate::dimse::StatusState::Success {
                                    crate::cget::CGetResponse::completed(&request, summary)
                                } else {
                                    let mut response = response;
                                    response.summary = summary;
                                    response
                                }
                            } else if response.state() == crate::dimse::StatusState::Success {
                                let summary = if response.summary.total() > 0 {
                                    response.summary
                                } else {
                                    provider_summary.unwrap_or(summary)
                                };
                                crate::cget::CGetResponse::completed(&request, summary)
                            } else {
                                let mut response = response;
                                if response.summary.total() == 0 {
                                    response.summary = provider_summary.unwrap_or(summary);
                                }
                                response
                            };
                            terminal.failed_sop_instance_uids.extend(failed_uids.clone());
        serving.send_with_dataset(context_id, &terminal.to_command(), terminal.identifier().as_ref(), syntax).await?;
                        }
                    }
                }
                if terminated {
                    break;
                }
            }
        }
    }
    if !terminated {
        let summary = if saw_instance {
            summary
        } else {
            provider_summary.unwrap_or(summary)
        };
        // A provider that reports progress but never sends a terminal response
        // still gets its terminal built from the counters (documented above).
        // An empty stream is different: it means the provider did nothing, and
        // declaring "0 sub-operations, success" would hide that (NET-09).
        let mut terminal = if summary.total() == 0 {
            crate::cget::CGetResponse::failure(
                &request,
                crate::dimse::status::PROCESSING_FAILURE,
                "C-GET provider ended without a response",
            )
        } else {
            crate::cget::CGetResponse::completed(&request, summary)
        };
        terminal
            .failed_sop_instance_uids
            .extend(failed_uids.clone());
        serving
            .send_with_dataset(
                context_id,
                &terminal.to_command(),
                terminal.identifier().as_ref(),
                syntax,
            )
            .await?;
    }
    Ok(())
}

/// The C-MOVE path: decode the Identifier and relay the provider's progress
/// (§11.4).
pub async fn handle_c_move<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    dataset_bytes: &[u8],
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let identifier = match crate::message::decode_dataset_with_fallback(
        dataset_bytes,
        syntax,
        serving.options.fallback_encoding,
    ) {
        Ok(identifier) => identifier,
        Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
    };

    // the destination AE title is mandatory (PS3.7 §9.3.2)
    let Some(destination_ae) = command.move_destination.clone() else {
        let request = crate::cmove::CMoveRequest {
            message_id: command.message_id,
            sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
            priority: command.priority,
            destination_ae: String::new(),
            identifier,
        };
        let response = crate::cmove::CMoveResponse::failure(
            &request,
            crate::dimse::status::MOVE_DESTINATION_UNKNOWN,
            "C-MOVE request has no Move Destination",
        );
        return serving
            .send_with_dataset(
                context_id,
                &response.to_command(),
                response.identifier().as_ref(),
                syntax,
            )
            .await;
    };

    if !is_valid_ae_title(&destination_ae) {
        let request = crate::cmove::CMoveRequest {
            message_id: command.message_id,
            sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
            priority: command.priority,
            destination_ae,
            identifier,
        };
        let response = crate::cmove::CMoveResponse::failure(
            &request,
            crate::dimse::status::MOVE_DESTINATION_UNKNOWN,
            "C-MOVE Move Destination is not a valid AE title",
        );
        return serving
            .send_with_dataset(
                context_id,
                &response.to_command(),
                response.identifier().as_ref(),
                syntax,
            )
            .await;
    }

    let request = crate::cmove::CMoveRequest {
        message_id: command.message_id,
        sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
        priority: command.priority,
        destination_ae,
        identifier,
    };

    // Sub-operation counters are cumulative per response (PS3.4 §C.4.2), so the
    // last Pending response carries the real progress; a provider that never
    // terminates its stream still gets an answer built from it.
    use futures::StreamExt;
    let provider = serving.provider.clone();
    let mut stream = provider.on_c_move(&request);
    let mut summary = crate::cmove::SubOperationSummary::default();
    let mut remaining = 0u16;
    let mut terminated = false;
    loop {
        if let Some(unit) = serving.pending_units.pop_front() {
            if unit.stream != Stream::Command {
                serving.defer(unit)?;
                continue;
            }
            let cancel = command_from_bytes(&unit.bytes)?;
            if cancel.field == CommandField::CCancelRq {
                if cancel.message_id_being_responded_to == Some(request.message_id)
                    && unit.context_id == context_id
                {
                    let mut response = crate::cmove::CMoveResponse::failure(
                        &request,
                        crate::dimse::status::CANCEL,
                        "C-MOVE cancelled",
                    );
                    response.remaining = remaining;
                    response.summary = summary;
                    serving
                        .send_with_dataset(
                            context_id,
                            &response.to_command(),
                            response.identifier().as_ref(),
                            syntax,
                        )
                        .await?;
                    return Ok(());
                }
                if serving.incoming_window > 1 {
                    serving.defer(unit)?;
                }
                continue;
            }
            serving.defer(unit)?;
            continue;
        }

        tokio::select! {
            biased;
            pdu = serving.next_pdu() => {
                let Some(pdu) = pdu? else { return Ok(()) };
                match pdu {
                    Pdu::PData(pdata) => {
                        let units = serving.push_pdata_offloaded(&pdata).await?;
                        serving.pending_units.extend(units);
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason });
                    }
                    Pdu::ReleaseRq => {
                        serving.send_pdu(&Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => return Err(NetworkError::protocol("unexpected PDU while C-MOVE is in flight")),
                }
            }
            response = stream.next() => {
                let Some(response) = response else { break };
                let response = match response {
                    Ok(response) => response,
                    Err(error) => crate::cmove::CMoveResponse::failure(
                        &request,
                        crate::dimse::status::PROCESSING_FAILURE,
                        error.to_string(),
                    ),
                };
                if response.is_pending() {
                    remaining = response.remaining;
                    summary = response.summary;
                } else {
                    terminated = true;
                }
                serving.send_with_dataset(context_id, &response.to_command(), response.identifier().as_ref(), syntax).await?;
                if terminated {
                    break;
                }
            }
        }
    }
    if !terminated {
        // Progress reported without a terminal response keeps the documented
        // "last Pending carries the result" behaviour; an empty stream does not
        // get that benefit, because it means nothing happened (NET-09).
        let response = if summary.total() == 0 {
            crate::cmove::CMoveResponse::failure(
                &request,
                crate::dimse::status::PROCESSING_FAILURE,
                "C-MOVE provider ended without a response",
            )
        } else {
            crate::cmove::CMoveResponse::completed(&request, summary)
        };
        serving
            .send_with_dataset(
                context_id,
                &response.to_command(),
                response.identifier().as_ref(),
                syntax,
            )
            .await?;
    }
    Ok(())
}

fn is_valid_ae_title(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty() && bytes.len() <= 16 && bytes.iter().all(|byte| (0x20..=0x7e).contains(byte))
}

/// The C-FIND path: decode the Identifier and stream the matches back (§11.4).
#[cfg(test)]
pub async fn handle_c_find<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    dataset_bytes: &[u8],
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let identifier = match crate::message::decode_dataset_with_fallback(
        dataset_bytes,
        syntax,
        serving.options.fallback_encoding,
    ) {
        Ok(identifier) => identifier,
        Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
    };

    let request = crate::cfind::CFindRequest {
        message_id: command.message_id,
        sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
        priority: command.priority,
        identifier,
    };

    // Clone the provider handle so its response stream can stay alive while
    // the association writes each response. This keeps the first Pending
    // response observable before a provider has finished producing results.
    use futures::StreamExt;
    let provider = serving.provider.clone();
    let mut stream = provider.on_c_find(&request);
    let mut matches = 0u16;
    let mut terminated = false;
    loop {
        if let Some(unit) = serving.pending_units.pop_front() {
            if unit.stream != Stream::Command {
                serving.defer(unit)?;
                continue;
            }
            let cancel = command_from_bytes(&unit.bytes)?;
            if cancel.field == CommandField::CCancelRq {
                if cancel.message_id_being_responded_to == Some(request.message_id)
                    && unit.context_id == context_id
                {
                    let response = crate::cfind::CFindResponse::failure(
                        &request,
                        crate::dimse::status::CANCEL,
                        "C-FIND cancelled",
                    );
                    serving.send(context_id, &response.to_command()).await?;
                    return Ok(());
                }
                if serving.incoming_window > 1 {
                    serving.defer(unit)?;
                }
                continue;
            }
            serving.defer(unit)?;
            continue;
        }
        tokio::select! {
            biased;
            // A cancel already waiting on the socket wins a simultaneous
            // provider response, preserving the wire order seen by the SCP.
            pdu = serving.next_pdu() => {
                let Some(pdu) = pdu? else { return Ok(()) };
                match pdu {
                    Pdu::PData(pdata) => {
                        let units = serving.push_pdata_offloaded(&pdata).await?;
                        serving.pending_units.extend(units);
                    }
                    Pdu::Abort(abort) => {
                        return Err(NetworkError::Abort { source_code: abort.source.code(), reason: abort.reason });
                    }
                    Pdu::ReleaseRq => {
                        serving.send_pdu(&Pdu::ReleaseRp).await?;
                        return Err(NetworkError::ClosedPrematurely { source: None });
                    }
                    _ => return Err(NetworkError::protocol("unexpected PDU while C-FIND is in flight")),
                }
            }
            response = stream.next() => {
                let Some(response) = response else { break };
                let response = match response {
                    Ok(response) => response,
                    Err(error) => crate::cfind::CFindResponse::failure(
                        &request,
                        crate::dimse::status::PROCESSING_FAILURE,
                        error.to_string(),
                    ),
                };
                let identifier = response.identifier.clone();
                if response.is_pending() {
                    matches = matches.saturating_add(1);
                } else {
                    matches = response.completed;
                    terminated = true;
                }
                serving.send_with_dataset(context_id, &response.to_command(), identifier.as_ref(), syntax).await?;
                if terminated {
                    break;
                }
            }
        }
    }
    if !terminated {
        // matches already sent keep the documented "a provider that never
        // terminates still gets a final answer"; no match at all is a provider
        // that did nothing and must not read as a successful empty query (NET-09).
        let mut response = if matches == 0 {
            crate::cfind::CFindResponse::failure(
                &request,
                crate::dimse::status::PROCESSING_FAILURE,
                "C-FIND provider ended without a response",
            )
        } else {
            crate::cfind::CFindResponse::success(&request, matches)
        };
        response.completed = matches;
        serving.send(context_id, &response.to_command()).await?;
    }
    Ok(())
}

/// The C-STORE path: spool the instance and hand it to the provider (§11.4).
#[cfg(test)]
pub async fn handle_c_store<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    body: MessageBody,
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if command.field != CommandField::CStoreRq {
        return serving
            .fail(
                context_id,
                &command,
                "dataset is only supported for C-STORE",
            )
            .await;
    }

    // parse failures become a failure status; the connection survives (§11.2).
    // The spool does synchronous file IO, so it runs on the blocking pool: a
    // slow disk must not stall this (or any other) connection's runtime task
    // (§11.2).
    let sink = serving.options.store_sink.clone();
    let syntax = syntax.clone();
    let fallback = serving.options.fallback_encoding;
    let instance = match tokio::task::spawn_blocking(move || {
        sink.materialize_body_with_fallback(&body, &syntax, fallback)
    })
    .await
    {
        Ok(Ok(instance)) => instance,
        Ok(Err(error)) => return serving.fail(context_id, &command, &error.to_string()).await,
        Err(join) => {
            let message = format!("spool task failed: {join}");
            return serving.fail(context_id, &command, &message).await;
        }
    };

    let request = match crate::store::c_store_request_from_message(&command, instance) {
        Ok(request) => request,
        Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
    };

    // PS3.4 §F.1: the dataset must describe the same instance as the command
    if let Some(comment) = crate::store::dataset_mismatch(&request) {
        let response = crate::services::CStoreResponse::failure(
            &request,
            crate::store::DATASET_MISMATCH,
            comment,
        );
        return serving.send(context_id, &response.to_command()).await;
    }

    let response = match serving.provider.on_c_store(&request).await {
        Ok(response) => response,
        Err(error) => crate::store::failure_response(&request, &error),
    };
    serving.send(context_id, &response.to_command()).await
}

/// The per-connection send side, so dispatch helpers stay small.
pub struct Serving<'a, P: ServiceProvider, S> {
    pub(crate) stream: &'a mut S,
    pub(crate) splitter: &'a PduSplitter,
    pub(crate) provider: &'a Arc<P>,
    pub(crate) options: &'a ServerOptions,
    /// Owned so a nested sub-operation can read the reverse C-STORE responses
    /// of its own C-GET (§11.4).
    pub(crate) assembler: PdvAssembler,
    /// Completed units that arrived in the same P-DATA-TF as a dispatched request.
    pub(crate) pending_units: VecDeque<MessageUnit>,
    pub(crate) deferred_units: VecDeque<MessageUnit>,
    pub(crate) deferred_requests: usize,
    pub(crate) incoming_window: usize,
    pub(crate) accepted: &'a AcceptedContexts,
    /// The next message ID for reverse sub-operations (§11.4).
    pub(crate) reverse_message_id: u16,
    /// Transcoder per (source, target) syntax pair, reused across the many
    /// sub-operations of one C-GET instead of rebuilt per instance.
    pub(crate) transcoder_cache:
        std::collections::HashMap<(String, String), radicom::pixel::transcoder::Transcoder>,
    /// Cancel-safe PDU framing: a half-read frame survives a `select!` that
    /// drops the read future (review #13).
    pub(crate) pdu_reader: crate::transport::PduReader,
}

impl<P: ServiceProvider, S> Serving<'_, P, S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    /// Cancel-safe PDU read: buffered bytes live in `pdu_reader`, so a
    /// `select!` that drops this future keeps any half-read frame for the next
    /// call instead of resynchronising mid-frame (review #13).
    /// Queues another negotiated request while a streaming service is active.
    pub(crate) fn defer(&mut self, unit: MessageUnit) -> Result<()> {
        if self.incoming_window <= 1 {
            return Err(NetworkError::protocol(
                "unexpected message while a request is in flight",
            ));
        }
        if unit.stream == Stream::Command {
            let command = command_from_bytes(&unit.bytes)?;
            self.accepted
                .validate_request(unit.context_id, &command, true)?;
            if command.field != CommandField::CCancelRq {
                if crate::dimse::response_field(command.field).is_none()
                    || self.deferred_requests + 1 >= self.incoming_window
                {
                    return Err(NetworkError::protocol(
                        "peer exceeded the negotiated asynchronous-operation window",
                    ));
                }
                self.deferred_requests += 1;
            }
        }
        // a spooled dataset counts its file length: parking it must not let a
        // peer fill the temp directory instead of memory (H3)
        let bytes: u64 = self
            .deferred_units
            .iter()
            .map(|unit| unit.body().len())
            .sum();
        if bytes.saturating_add(unit.body().len()) > PdvAssembler::DEFAULT_MAX_BUFFERED_BYTES as u64
        {
            return Err(NetworkError::protocol(
                "queued DIMSE messages exceed the maximum size",
            ));
        }
        self.deferred_units.push_back(unit);
        Ok(())
    }

    pub(crate) fn resume_deferred(&mut self) {
        self.deferred_units.append(&mut self.pending_units);
        std::mem::swap(&mut self.pending_units, &mut self.deferred_units);
        self.deferred_requests = 0;
    }

    /// Feeds a P-DATA-TF to the assembler, moving any spooling work onto the
    /// blocking pool so a dataset streaming to disk never blocks a runtime
    /// worker (§6, §11.1). Messages that stay in memory are assembled inline.
    pub(crate) async fn push_pdata_offloaded(
        &mut self,
        pdata: &crate::pdu::PData,
    ) -> Result<Vec<crate::assembler::MessageUnit>> {
        let spills = pdata
            .pdvs
            .iter()
            .any(|pdv| !pdv.is_command && self.assembler.is_spilling(pdv.context_id));
        if !spills {
            return self.assembler.push_pdata(pdata);
        }
        let mut assembler = std::mem::take(&mut self.assembler);
        let owned = pdata.clone();
        match tokio::task::spawn_blocking(move || {
            let result = assembler.push_pdata(&owned);
            (assembler, result)
        })
        .await
        {
            Ok((assembler, result)) => {
                self.assembler = assembler;
                result
            }
            Err(join) => {
                tracing::error!(target: "radicom::network", %join, "assembler blocking task failed");
                Err(NetworkError::protocol("assembler blocking task failed"))
            }
        }
    }

    pub(crate) async fn next_pdu(&mut self) -> Result<Option<Pdu>> {
        // The idle dispatcher waits unbounded: `request_timeout` governs only
        // responses to requests this SCP issues (see `next_pdu_with_deadline`).
        // Bounding the idle read by it would abort quiet-but-healthy peers.
        self.pdu_reader.read_pdu(self.stream).await
    }

    /// Reads the next PDU, bounded by `ServerOptions::request_timeout` (M8).
    ///
    /// Used for the responses to requests *this* SCP issues on the association
    /// (a reverse C-STORE sub-operation, an N-EVENT-REPORT after N-ACTION). The
    /// idle dispatcher keeps waiting unbounded, so a quiet peer is not a
    /// failure. The framer retains a half-read frame, so a dropped read is safe.
    async fn next_pdu_with_deadline(&mut self, message_id: u16) -> Result<Option<Pdu>> {
        match self.options.request_timeout {
            None => self.pdu_reader.read_pdu(self.stream).await,
            Some(limit) => {
                match tokio::time::timeout(limit, self.pdu_reader.read_pdu(self.stream)).await {
                    Ok(pdu) => pdu,
                    Err(_) => Err(NetworkError::RequestTimedOut { message_id }),
                }
            }
        }
    }

    /// Writes one PDU on the connection.
    async fn send_pdu(&mut self, pdu: &Pdu) -> Result<()> {
        write_pdu(self.stream, pdu).await
    }

    /// Sends one command-only message on `context_id`.
    async fn send(&mut self, context_id: u8, command: &Command) -> Result<()> {
        send_command(self.stream, self.splitter, context_id, command).await
    }

    /// Sends a command followed by an optional dataset (§11.1).
    async fn send_with_dataset(
        &mut self,
        context_id: u8,
        command: &Command,
        dataset: Option<&Dataset>,
        syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
    ) -> Result<()> {
        let command_bytes = bytes::Bytes::from(command_bytes(command)?);
        let payload = match dataset {
            Some(dataset) => Some(bytes::Bytes::from(crate::message::encode_dataset(
                dataset, syntax,
            )?)),
            None => None,
        };
        // Owned bytes handed straight to the splitter: each PDV is a shared
        // slice instead of a fresh copy (§11.2). One vectored write per message
        // instead of a flush per PDU (NET-12).
        let pdus = self
            .splitter
            .split_message_bytes(context_id, command_bytes, payload)?;
        write_pdata_batch(self.stream, pdus).await
    }

    /// Answers with ProcessingFailure and a truncated comment.
    async fn fail(&mut self, context_id: u8, request: &Command, comment: &str) -> Result<()> {
        let Some(mut response) =
            Command::response(request, crate::dimse::status::PROCESSING_FAILURE)
        else {
            return Err(NetworkError::protocol(
                "cannot answer a command that is not a request",
            ));
        };
        response.error_comment = Some(crate::dimse::truncate_lo(comment));
        self.send(context_id, &response).await
    }

    /// Reads PDUs until the C-STORE response for `message_id` arrives (§11.4).
    ///
    /// Cancellation uses the C-GET context; its C-STORE response must use the
    /// storage context. Other negotiated incoming requests are deferred.
    pub(crate) async fn await_store_response(
        &mut self,
        message_id: u16,
        storage_context_id: u8,
        operation_context_id: u8,
        operation_message_id: u16,
    ) -> Result<(Command, bool)> {
        let mut cancelled = false;
        loop {
            if let Some(unit) = self.pending_units.pop_front() {
                if unit.stream != Stream::Command {
                    if self.incoming_window > 1 {
                        self.defer(unit)?;
                    }
                    continue;
                }
                let command = command_from_bytes(&unit.bytes)?;
                if command.field == CommandField::CCancelRq
                    && command.message_id_being_responded_to == Some(operation_message_id)
                    && unit.context_id == operation_context_id
                {
                    cancelled = true;
                    continue;
                }
                if command.field == CommandField::CStoreRsp
                    && command.message_id_being_responded_to == Some(message_id)
                {
                    if unit.context_id != storage_context_id {
                        return Err(NetworkError::protocol(
                            "reverse C-STORE response used the wrong presentation context",
                        ));
                    }
                    return Ok((command, cancelled));
                }
                if self.incoming_window > 1 {
                    self.defer(unit)?;
                }
                continue;
            }
            // Read through the cancel-safe framer: the C-GET loop may have left
            // a half-read frame in it when a provider event won the `select!`
            // (NET-01). Reading the bare socket here would resynchronise
            // mid-frame.
            let Some(pdu) = self.next_pdu_with_deadline(message_id).await? else {
                return Err(NetworkError::ClosedPrematurely { source: None });
            };
            match pdu {
                Pdu::PData(pdata) => {
                    let units = self.push_pdata_offloaded(&pdata).await?;
                    self.pending_units.extend(units);
                }
                Pdu::Abort(abort) => {
                    return Err(NetworkError::Abort {
                        source_code: abort.source.code(),
                        reason: abort.reason,
                    })
                }
                Pdu::ReleaseRq => {
                    write_pdu(self.stream, &Pdu::ReleaseRp).await?;
                    return Err(NetworkError::ClosedPrematurely { source: None });
                }
                _ => {
                    return Err(NetworkError::protocol(
                        "unexpected PDU while waiting for reverse C-STORE",
                    ))
                }
            }
        }
    }

    /// Parks a unit for the outer dispatcher (H2).
    ///
    /// Used while this side waits for a response: the unit cannot be handled
    /// re-entrantly, and [`Self::resume_deferred`] moves it back into the main
    /// queue once the request completes.
    fn park(&mut self, unit: MessageUnit) -> Result<()> {
        let bytes: u64 = self
            .deferred_units
            .iter()
            .map(|unit| unit.body().len())
            .sum();
        if bytes.saturating_add(unit.body().len()) > PdvAssembler::DEFAULT_MAX_BUFFERED_BYTES as u64
        {
            return Err(NetworkError::protocol(
                "queued DIMSE messages exceed the maximum size",
            ));
        }
        self.deferred_units.push_back(unit);
        Ok(())
    }

    /// Reads the response to an N-service request *this* side sent (H2).
    ///
    /// Bounded by `ServerOptions::request_timeout` (M8). Units that are not the
    /// awaited response are parked for the outer dispatcher.
    async fn await_n_response(
        &mut self,
        message_id: u16,
        expected: CommandField,
        syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
    ) -> Result<(Command, Option<Dataset>)> {
        let mut pending_command: Option<Command> = None;
        loop {
            if let Some(unit) = self.pending_units.pop_front() {
                match unit.stream {
                    Stream::Command => {
                        let command = command_from_bytes(&unit.bytes)?;
                        if command.field == expected
                            && command.message_id_being_responded_to == Some(message_id)
                        {
                            if command.no_dataset {
                                return Ok((command, None));
                            }
                            pending_command = Some(command);
                            continue;
                        }
                        self.park(unit)?;
                    }
                    Stream::Data => {
                        if let Some(command) = pending_command.take() {
                            let dataset = crate::message::decode_dataset_with_fallback(
                                &unit.bytes,
                                syntax,
                                self.options.fallback_encoding,
                            )?;
                            return Ok((command, Some(dataset)));
                        }
                        self.park(unit)?;
                    }
                }
                continue;
            }
            let Some(pdu) = self.next_pdu_with_deadline(message_id).await? else {
                return Err(NetworkError::ClosedPrematurely { source: None });
            };
            match pdu {
                Pdu::PData(pdata) => {
                    let units = self.push_pdata_offloaded(&pdata).await?;
                    self.pending_units.extend(units);
                }
                Pdu::Abort(abort) => {
                    return Err(NetworkError::Abort {
                        source_code: abort.source.code(),
                        reason: abort.reason,
                    })
                }
                Pdu::ReleaseRq => {
                    write_pdu(self.stream, &Pdu::ReleaseRp).await?;
                    return Err(NetworkError::ClosedPrematurely { source: None });
                }
                _ => {
                    return Err(NetworkError::protocol(
                        "unexpected PDU while waiting for an N-service response",
                    ))
                }
            }
        }
    }

    /// Sends the N-EVENT-REPORT requests a provider queued after N-ACTION (H2).
    ///
    /// The peer sees SCP-initiated reports on the same association.
    ///
    /// **Ceiling:** a report is only legal when the peer granted this side the
    /// SCU role for the service class (PS3.7 §10.1). Without that grant the
    /// report is skipped rather than failing the association: the N-ACTION was
    /// answered already, and a peer that rejected the role never agreed to be
    /// notified. A report with no SOP class UID is a provider bug and fails.
    async fn send_n_event_reports(
        &mut self,
        action: &crate::nservice::NServiceRequest,
    ) -> Result<()> {
        let events = self.provider.on_send_n_event_report(action).await?;
        for mut event in events {
            let Some(sop_class) = event.sop_class_uid.clone() else {
                return Err(NetworkError::protocol(
                    "an N-EVENT-REPORT request has no SOP class UID",
                ));
            };
            if !self.accepted.peer_holds_scp_role(&sop_class) {
                continue;
            }
            let context = self.accepted.pick(&sop_class, "")?;
            let context_id = context.id;
            let target = transfer_syntax_of(&context.transfer_syntax)?;
            let message_id = self.next_reverse_message_id();
            event.message_id = message_id;
            let expected = crate::dimse::response_field(event.field)
                .ok_or_else(|| NetworkError::protocol("N-EVENT-REPORT has no response field"))?;
            let command = event.to_command();
            self.send_with_dataset(context_id, &command, event.dataset.as_ref(), &target)
                .await?;
            // the response is informational: the N-ACTION was answered already,
            // so a rejected report must not fail the association
            let _ = self.await_n_response(message_id, expected, &target).await?;
        }
        Ok(())
    }

    /// Delivers one instance as a reverse C-STORE sub-operation (§11.4).
    ///
    /// The SCU side must have registered a C-STORE handler, otherwise this is
    /// `ReverseStoreHandlerMissing` on the *caller*; the SCP just records the
    /// returned status. The boolean result reports whether C-CANCEL arrived
    /// while this reverse sub-operation was completing.
    pub(crate) async fn send_reverse_store(
        &mut self,
        file: &radicom::io::file::DicomFile,
        operation_context_id: u8,
        operation_message_id: u16,
    ) -> Result<(StatusState, bool)> {
        let dataset = &file.dataset;
        let sop_class = dataset
            .try_get_str(Tag::SOP_CLASS_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
        let sop_instance = dataset
            .try_get_str(Tag::SOP_INSTANCE_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP instance UID"))?;
        let source_syntax = dataset.transfer_syntax();
        let context = self.accepted.pick(&sop_class, source_syntax.uid())?;
        let context_id = context.id;
        // A peer that negotiated roles only accepts C-STORE from a side holding
        // the SCU role, which this side gets from the SCP role the peer was
        // granted; sending anyway would be a protocol violation (PS3.7 §7.4,
        // review #16). Default roles do not grant the requestor Storage SCP.
        if !self.accepted.peer_holds_scp_role(&sop_class) {
            return Err(NetworkError::ScuRoleNotNegotiated { sop_class });
        }
        let target = transfer_syntax_of(&context.transfer_syntax)?;

        let message_id = self.next_reverse_message_id();
        let mut request = Command::request(CommandField::CStoreRq, message_id);
        request.affected_sop_class_uid = Some(sop_class);
        request.affected_sop_instance_uid = Some(sop_instance);
        request.no_dataset = false;

        let command = bytes::Bytes::from(command_bytes(&request)?);
        // The syntax the dataset is actually written in: the negotiated target,
        // or the instance's own syntax when no codec can convert it and the
        // caller allows that (L3, fo-dicom IgnoreUnsupportedTransferSyntaxChange).
        let (send_syntax, transcoded) = if source_syntax.uid() == target.uid() {
            (target.clone(), None)
        } else {
            let transcoder = transcoder_for(&mut self.transcoder_cache, &source_syntax, &target);
            match transcoder.transcode_dataset(dataset) {
                Ok(transcoded) => (target.clone(), Some(transcoded)),
                Err(error) if self.options.ignore_unsupported_syntax_change => {
                    // PixelData is not stripped: the peer gets the bytes in the
                    // syntax they are in, and decides what to do with them
                    let _ = error;
                    (source_syntax.clone(), None)
                }
                Err(error) => {
                    return Err(NetworkError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        error.to_string(),
                    )))
                }
            }
        };
        match transcoded {
            None => {
                // Stream when no transcode was needed (NET-13): the writer frames
                // PDUs through a bounded channel
                let max_pdu = self.splitter.max_pdu();
                let max_pdvs = self.splitter.max_pdvs_per_pdu();
                crate::message::write_message_streamed(
                    self.stream,
                    context_id,
                    command,
                    dataset.clone(),
                    send_syntax,
                    max_pdu,
                    max_pdvs,
                    self.splitter.max_command_pdv(),
                    self.splitter.max_data_pdv(),
                )
                .await?;
            }
            Some(transcoded) => {
                let payload =
                    bytes::Bytes::from(crate::message::encode_dataset(&transcoded, &send_syntax)?);
                // one vectored write for the whole sub-operation (NET-12)
                let pdus = self
                    .splitter
                    .split_message_bytes(context_id, command, Some(payload))?;
                write_pdata_batch(self.stream, pdus).await?;
            }
        }

        let (response, cancelled) = self
            .await_store_response(
                message_id,
                context_id,
                operation_context_id,
                operation_message_id,
            )
            .await?;
        for (actual, expected) in [
            (
                &response.affected_sop_class_uid,
                &request.affected_sop_class_uid,
            ),
            (
                &response.affected_sop_instance_uid,
                &request.affected_sop_instance_uid,
            ),
        ] {
            if actual.is_some() && actual != expected {
                return Err(NetworkError::protocol(
                    "reverse C-STORE response SOP UID does not match its request",
                ));
            }
        }
        Ok((
            crate::dimse::classify(
                response
                    .status
                    .unwrap_or(crate::dimse::status::PROCESSING_FAILURE),
            ),
            cancelled,
        ))
    }

    /// Message IDs for reverse sub-operations; the SCP side of a C-GET owns
    /// this sequence because it is the one issuing requests (§11.4).
    fn next_reverse_message_id(&mut self) -> u16 {
        let id = self.reverse_message_id;
        self.reverse_message_id = if self.reverse_message_id == u16::MAX {
            1
        } else {
            self.reverse_message_id + 1
        };
        id
    }
}

/// The hard cap on concurrent request tasks per association (M6).
///
/// The negotiated window usually bounds this first; the cap keeps a peer that
/// claims an unlimited window from spawning without bound.
pub const MAX_CONCURRENT_REQUESTS: usize = 64;

/// One request the message loop hands to a task (M6).
///
/// Only requests whose answer needs no further read from this association are
/// dispatched: C-ECHO, C-STORE, C-FIND and the N-services other than N-ACTION.
/// C-GET and C-MOVE stay on the loop because they interleave sub-operations
/// with reads of this socket, and N-ACTION stays because it may push
/// N-EVENT-REPORTs back over it (see the ceiling notes on [`serve_connection`]).
pub struct DispatchJob {
    pub(crate) context_id: u8,
    pub(crate) command: Command,
    /// Empty for a request that carries no dataset.
    pub(crate) body: MessageBody,
    pub(crate) syntax: Arc<radicom::model::transfer_syntax::TransferSyntax>,
    /// Present for a C-FIND: the peer may cancel it with C-CANCEL (SRV-08).
    pub(crate) cancel: Option<CancelSlot>,
}

/// The cancel table and token of one running request (M6).
///
/// Dropping the slot removes the map entry, so the table only holds requests
/// that are still running.
pub type CancelMap =
    Arc<std::sync::Mutex<std::collections::HashMap<u16, tokio::sync::watch::Sender<bool>>>>;

pub struct CancelSlot {
    map: CancelMap,
    id: u16,
    rx: tokio::sync::watch::Receiver<bool>,
}

impl CancelSlot {
    fn register(map: &CancelMap, id: u16) -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        map.lock().unwrap().insert(id, tx);
        CancelSlot {
            map: map.clone(),
            id,
            rx,
        }
    }

    /// Resolves when the message loop forwards a C-CANCEL for this request.
    async fn cancelled(&mut self) {
        if *self.rx.borrow() {
            return;
        }
        let _ = self.rx.changed().await;
    }
}

impl Drop for CancelSlot {
    fn drop(&mut self) {
        self.map.lock().unwrap().remove(&self.id);
    }
}

impl DispatchJob {
    /// True when `command` may run on a task instead of the message loop (M6).
    fn is_concurrent(command: &Command) -> bool {
        match command.field {
            CommandField::CEchoRq | CommandField::CStoreRq | CommandField::CFindRq => true,
            // N-ACTION may send N-EVENT-REPORTs on this same association (H2)
            CommandField::NActionRq => false,
            field => crate::nservice::is_n_request(field),
        }
    }
}

/// A response a dispatched task produced, ready for the loop to write.
///
/// The loop encodes and writes it in one go, so PDVs of different messages can
/// never interleave on the wire (§11.1) and the response keeps the request's
/// message ID whatever order the tasks finish in.
pub struct Outgoing {
    pub(crate) context_id: u8,
    pub(crate) command: Command,
    pub(crate) dataset: Option<Dataset>,
    pub(crate) syntax: Arc<radicom::model::transfer_syntax::TransferSyntax>,
}

/// A ProcessingFailure answer for `request` (§11.3).
pub fn failure_outgoing(
    context_id: u8,
    request: &Command,
    comment: &str,
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Outgoing {
    let mut command = Command::response(request, crate::dimse::status::PROCESSING_FAILURE)
        .unwrap_or_else(|| Command::request(request.field, request.message_id));
    command.error_comment = Some(crate::dimse::truncate_lo(comment));
    Outgoing {
        context_id,
        command,
        dataset: None,
        syntax: syntax.clone(),
    }
}

/// Runs one dispatched request and streams its responses to the loop (M6).
///
/// An error from the provider becomes a failure response, exactly like the
/// inline handlers; the association itself survives it.
pub async fn run_job<P: ServiceProvider + 'static>(
    job: DispatchJob,
    provider: Arc<P>,
    options: ServerOptions,
    tx: tokio::sync::mpsc::Sender<Result<Outgoing>>,
) {
    if let Err(error) = execute_job(job, &provider, &options, &tx).await {
        // the loop is gone when the channel is closed: nothing left to report to
        let _ = tx.send(Err(error)).await;
    }
}

pub async fn execute_job<P: ServiceProvider>(
    mut job: DispatchJob,
    provider: &Arc<P>,
    options: &ServerOptions,
    tx: &tokio::sync::mpsc::Sender<Result<Outgoing>>,
) -> Result<()> {
    match job.command.field {
        CommandField::CEchoRq => execute_c_echo(job, provider, tx).await,
        CommandField::CStoreRq => execute_c_store(job, provider, options, tx).await,
        CommandField::CFindRq => execute_c_find(&mut job, provider, options, tx).await,
        field if crate::nservice::is_n_request(field) => {
            execute_n_service(job, provider, options, tx).await
        }
        other => {
            let comment = format!("not a concurrent request: {other:?}");
            let outgoing = failure_outgoing(job.context_id, &job.command, &comment, &job.syntax);
            let _ = tx.send(Ok(outgoing)).await;
            Ok(())
        }
    }
}

pub async fn execute_c_echo<P: ServiceProvider>(
    job: DispatchJob,
    provider: &Arc<P>,
    tx: &tokio::sync::mpsc::Sender<Result<Outgoing>>,
) -> Result<()> {
    let request = CEchoRequest::from_command(&job.command)?;
    let outgoing = match provider.on_c_echo(&request).await {
        Ok(response) => Outgoing {
            context_id: job.context_id,
            command: response.to_command(),
            dataset: None,
            syntax: job.syntax.clone(),
        },
        Err(error) => {
            let comment = format!("C-ECHO failed: {error}");
            failure_outgoing(job.context_id, &job.command, &comment, &job.syntax)
        }
    };
    let _ = tx.send(Ok(outgoing)).await;
    Ok(())
}

pub async fn execute_c_store<P: ServiceProvider>(
    job: DispatchJob,
    provider: &Arc<P>,
    options: &ServerOptions,
    tx: &tokio::sync::mpsc::Sender<Result<Outgoing>>,
) -> Result<()> {
    let DispatchJob {
        context_id,
        command,
        body,
        syntax,
        // C-STORE and the N-services are not cancellable (§11.4)
        cancel: _,
    } = job;
    // the spool is synchronous file IO: keep it off the runtime thread
    let sink = options.store_sink.clone();
    let spool_syntax = syntax.clone();
    let fallback = options.fallback_encoding;
    let instance = match tokio::task::spawn_blocking(move || {
        sink.materialize_body_with_fallback(&body, &spool_syntax, fallback)
    })
    .await
    {
        Ok(Ok(instance)) => instance,
        Ok(Err(error)) => {
            let comment = error.to_string();
            let outgoing = failure_outgoing(context_id, &command, &comment, &syntax);
            let _ = tx.send(Ok(outgoing)).await;
            return Ok(());
        }
        Err(join) => {
            let comment = format!("spool task failed: {join}");
            let outgoing = failure_outgoing(context_id, &command, &comment, &syntax);
            let _ = tx.send(Ok(outgoing)).await;
            return Ok(());
        }
    };

    let request = match crate::store::c_store_request_from_message(&command, instance) {
        Ok(request) => request,
        Err(error) => {
            let comment = error.to_string();
            let outgoing = failure_outgoing(context_id, &command, &comment, &syntax);
            let _ = tx.send(Ok(outgoing)).await;
            return Ok(());
        }
    };

    // PS3.4 §F.1: the dataset must describe the same instance as the command
    if let Some(comment) = crate::store::dataset_mismatch(&request) {
        let response = crate::services::CStoreResponse::failure(
            &request,
            crate::store::DATASET_MISMATCH,
            comment,
        );
        let outgoing = Outgoing {
            context_id,
            command: response.to_command(),
            dataset: None,
            syntax,
        };
        let _ = tx.send(Ok(outgoing)).await;
        return Ok(());
    }

    let response = match provider.on_c_store(&request).await {
        Ok(response) => response,
        Err(error) => crate::store::failure_response(&request, &error),
    };
    let outgoing = Outgoing {
        context_id,
        command: response.to_command(),
        dataset: None,
        syntax,
    };
    let _ = tx.send(Ok(outgoing)).await;
    Ok(())
}

pub async fn execute_c_find<P: ServiceProvider>(
    job: &mut DispatchJob,
    provider: &Arc<P>,
    options: &ServerOptions,
    tx: &tokio::sync::mpsc::Sender<Result<Outgoing>>,
) -> Result<()> {
    use futures::StreamExt;
    let context_id = job.context_id;
    let command = job.command.clone();
    let syntax = job.syntax.clone();
    let dataset_bytes = job.body.read_all()?;
    let identifier = match crate::message::decode_dataset_with_fallback(
        &dataset_bytes,
        &syntax,
        options.fallback_encoding,
    ) {
        Ok(identifier) => identifier,
        Err(error) => {
            let comment = error.to_string();
            let outgoing = failure_outgoing(context_id, &command, &comment, &syntax);
            let _ = tx.send(Ok(outgoing)).await;
            return Ok(());
        }
    };
    let request = crate::cfind::CFindRequest {
        message_id: command.message_id,
        sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
        priority: command.priority,
        identifier,
    };

    let mut stream = provider.on_c_find(&request);
    let mut matches = 0u16;
    let mut terminated = false;
    loop {
        let next = match &mut job.cancel {
            Some(cancel) => tokio::select! {
                biased;
                // SRV-08: a C-CANCEL forwarded by the message loop wins a
                // simultaneous provider response, preserving wire order
                _ = cancel.cancelled() => {
                    let response = crate::cfind::CFindResponse::failure(
                        &request,
                        crate::dimse::status::CANCEL,
                        "C-FIND cancelled",
                    );
                    let outgoing = Outgoing {
                        context_id,
                        command: response.to_command(),
                        dataset: None,
                        syntax: syntax.clone(),
                    };
                    let _ = tx.send(Ok(outgoing)).await;
                    return Ok(());
                }
                response = stream.next() => response,
            },
            None => stream.next().await,
        };
        let Some(response) = next else { break };
        let response = match response {
            Ok(response) => response,
            Err(error) => crate::cfind::CFindResponse::failure(
                &request,
                crate::dimse::status::PROCESSING_FAILURE,
                error.to_string(),
            ),
        };
        let identifier = response.identifier.clone();
        if response.is_pending() {
            matches = matches.saturating_add(1);
        } else {
            matches = response.completed;
            terminated = true;
        }
        let outgoing = Outgoing {
            context_id,
            command: response.to_command(),
            dataset: identifier,
            syntax: syntax.clone(),
        };
        let _ = tx.send(Ok(outgoing)).await;
        if terminated {
            break;
        }
    }
    if !terminated {
        // a provider that never terminates still gets a final answer; a
        // provider that produced nothing must not read as an empty match (NET-09)
        let mut response = if matches == 0 {
            crate::cfind::CFindResponse::failure(
                &request,
                crate::dimse::status::PROCESSING_FAILURE,
                "C-FIND provider ended without a response",
            )
        } else {
            crate::cfind::CFindResponse::success(&request, matches)
        };
        response.completed = matches;
        let outgoing = Outgoing {
            context_id,
            command: response.to_command(),
            dataset: None,
            syntax,
        };
        let _ = tx.send(Ok(outgoing)).await;
    }
    Ok(())
}
pub async fn execute_n_service<P: ServiceProvider>(
    job: DispatchJob,
    provider: &Arc<P>,
    options: &ServerOptions,
    tx: &tokio::sync::mpsc::Sender<Result<Outgoing>>,
) -> Result<()> {
    use crate::nservice::NServiceRequest;
    let DispatchJob {
        context_id,
        command,
        body,
        syntax,
        // C-STORE and the N-services are not cancellable (§11.4)
        cancel: _,
    } = job;
    let dataset = if body.is_empty() {
        None
    } else {
        let dataset_bytes = body.read_all()?;
        match crate::message::decode_dataset_with_fallback(
            &dataset_bytes,
            &syntax,
            options.fallback_encoding,
        ) {
            Ok(dataset) => Some(dataset),
            Err(error) => {
                let comment = error.to_string();
                let outgoing = failure_outgoing(context_id, &command, &comment, &syntax);
                let _ = tx.send(Ok(outgoing)).await;
                return Ok(());
            }
        }
    };
    let request = NServiceRequest::from_command(&command, dataset);
    let outcome = match request.field {
        CommandField::NEventReportRq => provider.on_n_event_report(&request).await,
        CommandField::NGetRq => provider.on_n_get(&request).await,
        CommandField::NSetRq => provider.on_n_set(&request).await,
        CommandField::NCreateRq => provider.on_n_create(&request).await,
        // N-ACTION is dispatched inline (it may push N-EVENT-REPORTs), and
        // N-DELETE has no dataset: both answered the same way here
        CommandField::NDeleteRq => provider.on_n_delete(&request).await,
        other => {
            let comment = format!("not an N-service: {other:?}");
            let outgoing = failure_outgoing(context_id, &command, &comment, &syntax);
            let _ = tx.send(Ok(outgoing)).await;
            return Ok(());
        }
    };
    let outgoing = match outcome {
        Ok(response) => Outgoing {
            context_id,
            command: response.to_command(),
            dataset: response.dataset.clone(),
            syntax,
        },
        Err(error) => {
            let comment = error.to_string();
            failure_outgoing(context_id, &command, &comment, &syntax)
        }
    };
    let _ = tx.send(Ok(outgoing)).await;
    Ok(())
}

/// The largest PDU body this server will send to a peer: the peer's advertised
/// Max-PDU-Length clamped to [`crate::pdu::MAX_PDU_SIZE`]. A value of 0 means
/// "no maximum" on the wire (PS3.8 §9.3.2), which this implementation cannot
/// honour, so it falls back to our own configured ceiling. Without the clamp a
/// hostile `Max-PDU-Length = 0xFFFF_FFFF` would make the streaming framer
/// pre-allocate a ~4 GiB buffer.
pub fn negotiated_send_max_pdu(peer_advertised: u32, local_max_pdu: u32) -> u32 {
    if peer_advertised == 0 {
        crate::pdu::clamp_max_pdu_length(local_max_pdu)
    } else {
        crate::pdu::clamp_max_pdu_length(peer_advertised)
    }
}

/// Resolves a negotiated transfer-syntax UID to its syntax.
///
/// The UID always comes from an accepted presentation context (validated
/// against our supported list at bind time, or the AC verified against what we
/// proposed), so an unknown UID is an invariant violation: fail loudly rather
/// than silently decoding with the wrong syntax.
pub fn transfer_syntax_of(
    uid: &str,
) -> Result<Arc<radicom::model::transfer_syntax::TransferSyntax>> {
    radicom::model::transfer_syntax::parse(uid)
        .map_err(|_| NetworkError::protocol("accepted transfer syntax is not supported"))
}

/// Validates the presentation contexts of an A-ASSOCIATE-RQ (§9.3.2):
/// identifiers must be odd, non-zero and unique, and both syntaxes must be
/// present. An RQ has no Result/Reason field (the third byte is reserved and
/// must not be tested, PS3.8 Table 9-13).
pub fn validate_proposed_contexts(contexts: &[PresentationContext]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for pc in contexts {
        if pc.id == 0 || pc.id % 2 == 0 {
            return Err(NetworkError::protocol(
                "presentation-context ID must be odd and non-zero",
            ));
        }
        if !seen.insert(pc.id) {
            return Err(NetworkError::protocol(
                "duplicate presentation-context ID in the association request",
            ));
        }
        if pc.abstract_syntax.is_empty() {
            return Err(NetworkError::protocol(
                "presentation context has no abstract syntax",
            ));
        }
        if pc.transfer_syntaxes.is_empty() {
            return Err(NetworkError::protocol(
                "presentation context has no transfer syntax",
            ));
        }
    }
    Ok(())
}

/// Writes one DIMSE message's P-DATA PDUs in a single `write_all` (§11.2).
pub async fn write_pdata_batch<S>(stream: &mut S, pdus: Vec<crate::pdu::PData>) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let pdus: Vec<Pdu> = pdus.into_iter().map(Pdu::PData).collect();
    write_pdus(stream, &pdus).await
}

/// Returns the cached transcoder for a (source, target) syntax pair, building
/// it once. A C-GET may transcode hundreds of instances with the same pair.
pub fn transcoder_for<'a>(
    cache: &'a mut std::collections::HashMap<
        (String, String),
        radicom::pixel::transcoder::Transcoder,
    >,
    source: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
    target: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> &'a radicom::pixel::transcoder::Transcoder {
    let key = (source.uid().to_string(), target.uid().to_string());
    cache.entry(key).or_insert_with(|| {
        radicom::pixel::transcoder::Transcoder::new(source.clone(), target.clone(), None, None)
    })
}

/// Dispatches a command that carries no dataset (§11.1).
pub async fn handle_command<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match command.field {
        CommandField::CEchoRq => {
            let request = CEchoRequest::from_command(&command)?;
            // a provider error is a failed operation, not a broken association
            // (the C-STORE and N-service paths already answer a failure status)
            match serving.provider.on_c_echo(&request).await {
                Ok(response) => serving.send(context_id, &response.to_command()).await,
                Err(error) => {
                    serving
                        .fail(context_id, &command, &format!("C-ECHO failed: {error}"))
                        .await
                }
            }
        }
        CommandField::CCancelRq => {
            // §11.4 / SRV-08: streaming C-FIND handles its own cancel while
            // waiting for provider output. A cancel received by the idle
            // dispatcher has no matching in-flight operation, so consume it
            // and keep the association healthy.
            let _cancelled = command.message_id_being_responded_to;
            Ok(())
        }
        // N-services without a dataset (N-GET / N-DELETE, and any N request
        // sent with the no-dataset flag set) — §11.4 / SRV-07.
        field if crate::nservice::is_n_request(field) => {
            // responses are written on the request's own context, so they use
            // that context's accepted syntax (review #15)
            let syntax = match serving.accepted.by_id(context_id) {
                Some(c) => transfer_syntax_of(&c.transfer_syntax)?,
                None => syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            };
            handle_n_service(serving, context_id, command, None, &syntax).await
        }
        other => {
            // A response field means the peer violated DIMSE: there is no
            // status to answer a response with. Abort instead of panicking.
            if crate::dimse::response_field(other).is_none() {
                let abort = Abort {
                    source: AbortSource::User,
                    reason: 0,
                };
                serving.send_pdu(&Pdu::Abort(abort)).await?;
                return Err(NetworkError::PeerViolation(
                    "peer sent a command that is not a request",
                ));
            }
            // a request we do not serve: answer ProcessingFailure, keep the connection
            serving
                .fail(
                    context_id,
                    &command,
                    &format!("unsupported command {other:?}"),
                )
                .await
        }
    }
}

/// Dispatches a command together with its dataset — the C-STORE path (§11.4).
pub async fn handle_command_with_dataset<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    body: MessageBody,
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match command.field {
        // C-STORE and C-FIND are always dispatched to a task
        // (`DispatchJob::is_concurrent`); a dataset arriving here for them is a
        // dispatch bug. Keeping the single `execute_c_store`/`execute_c_find`
        // implementation avoids two divergent copies of the validation logic.
        CommandField::CStoreRq | CommandField::CFindRq => Err(NetworkError::protocol(
            "C-STORE/C-FIND reached the inline dataset dispatcher",
        )),
        CommandField::CMoveRq => {
            let dataset_bytes = body.read_all()?;
            handle_c_move(serving, context_id, command, &dataset_bytes, syntax).await
        }
        CommandField::CGetRq => {
            let dataset_bytes = body.read_all()?;
            handle_c_get(serving, context_id, command, &dataset_bytes, syntax).await
        }
        field if crate::nservice::is_n_request(field) => {
            let dataset_bytes = body.read_all()?;
            let dataset = match crate::message::decode_dataset_with_fallback(
                &dataset_bytes,
                syntax,
                serving.options.fallback_encoding,
            ) {
                Ok(dataset) => dataset,
                Err(error) => return serving.fail(context_id, &command, &error.to_string()).await,
            };
            handle_n_service(serving, context_id, command, Some(dataset), syntax).await
        }
        _ => {
            serving
                .fail(
                    context_id,
                    &command,
                    "this request does not carry a dataset",
                )
                .await
        }
    }
}

/// Dispatches an N-service request to the matching provider method (§11.4, SRV-07).
pub async fn handle_n_service<P: ServiceProvider, S>(
    serving: &mut Serving<'_, P, S>,
    context_id: u8,
    command: Command,
    dataset: Option<Dataset>,
    syntax: &Arc<radicom::model::transfer_syntax::TransferSyntax>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use crate::nservice::NServiceRequest;
    let request = NServiceRequest::from_command(&command, dataset);
    let outcome = match request.field {
        CommandField::NEventReportRq => serving.provider.on_n_event_report(&request).await,
        CommandField::NGetRq => serving.provider.on_n_get(&request).await,
        CommandField::NSetRq => serving.provider.on_n_set(&request).await,
        CommandField::NActionRq => serving.provider.on_n_action(&request).await,
        CommandField::NCreateRq => serving.provider.on_n_create(&request).await,
        CommandField::NDeleteRq => serving.provider.on_n_delete(&request).await,
        other => {
            return serving
                .fail(
                    context_id,
                    &command,
                    &format!("not an N-service: {other:?}"),
                )
                .await;
        }
    };
    match outcome {
        Ok(response) => {
            serving
                .send_with_dataset(
                    context_id,
                    &response.to_command(),
                    response.dataset.as_ref(),
                    syntax,
                )
                .await?;
            // H2: the SCP of the service class answers N-ACTION and may then
            // report the event on this same association (fo-dicom
            // `IDicomNEventReportRequestProvider`).
            if request.field == CommandField::NActionRq {
                serving.send_n_event_reports(&request).await?;
            }
            Ok(())
        }
        // a provider error becomes a ProcessingFailure, keeping the association alive
        Err(error) => serving.fail(context_id, &command, &error.to_string()).await,
    }
}

/// True for failures that mean the association itself is unusable.
///
/// Everything else — a missing SOP class, an instance without the UIDs needed
/// to address it, a transcode that cannot run — describes one sub-operation,
/// so C-GET records it as failed and carries on with the next instance.
pub fn is_fatal(error: &NetworkError) -> bool {
    matches!(
        error,
        NetworkError::Io(_) | NetworkError::Abort { .. } | NetworkError::ClosedPrematurely { .. }
    )
}

/// Sends a command-only message (§11.1).
pub async fn send_command<S>(
    stream: &mut S,
    splitter: &PduSplitter,
    context_id: u8,
    command: &Command,
) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let bytes = bytes::Bytes::from(command_bytes(command)?);
    let pdus = splitter.split_bytes(context_id, true, bytes)?;
    write_pdata_batch(stream, pdus).await
}
