/*-------------------------------------------------------------------------
 *
 * walproposer.c
 *
 * Proposer/leader part of the total order broadcast protocol between postgres
 * and WAL safekeepers.
 *
 * We have two ways of launching WalProposer:
 *
 *   1. As a background worker which will pretend to be physical WalSender.
 * 		WalProposer will receive notifications about new available WAL and
 * 		will immediately broadcast it to alive safekeepers.
 *
 *   2. As a standalone utility by running `postgres --sync-safekeepers`. That
 *      is needed to create LSN from which it is safe to start postgres. More
 *      specifically it addresses following problems:
 *
 *      a) Chicken-or-the-egg problem: compute postgres needs data directory
 *         with non-rel files that are downloaded from pageserver by calling
 *         basebackup@LSN. This LSN is not arbitrary, it must include all
 *         previously committed transactions and defined through consensus
 *         voting, which happens... in walproposer, a part of compute node.
 *
 *      b) Just warranting such LSN is not enough, we must also actually commit
 *         it and make sure there is a safekeeper who knows this LSN is
 *         committed so WAL before it can be streamed to pageserver -- otherwise
 *         basebackup will hang waiting for WAL. Advancing commit_lsn without
 *         playing consensus game is impossible, so speculative 'let's just poll
 *         safekeepers, learn start LSN of future epoch and run basebackup'
 *         won't work.
 *
 * Both ways are implemented in walproposer_pg.c file. This file contains
 * generic part of walproposer which can be used in both cases, but can also
 * be used as an independent library.
 *
 *-------------------------------------------------------------------------
 */
#include <sys/resource.h>

#include "postgres.h"
#include "libpq/pqformat.h"
#include "neon.h"
#include "walproposer.h"
#include "neon_utils.h"

/* Prototypes for private functions */
static void WalProposerLoop(WalProposer *wp);
static void ShutdownConnection(Safekeeper *sk);
static void ResetConnection(Safekeeper *sk);
static long TimeToReconnect(WalProposer *wp, TimestampTz now);
static void ReconnectSafekeepers(WalProposer *wp);
static void AdvancePollState(Safekeeper *sk, uint32 events);
static void HandleConnectionEvent(Safekeeper *sk);
static void SendStartWALPush(Safekeeper *sk);
static void RecvStartWALPushResult(Safekeeper *sk);
static void SendProposerGreeting(Safekeeper *sk);
static void RecvAcceptorGreeting(Safekeeper *sk);
static void SendVoteRequest(Safekeeper *sk);
static void RecvVoteResponse(Safekeeper *sk);
static bool VotesCollected(WalProposer *wp);
static void HandleElectedProposer(WalProposer *wp);
static term_t GetHighestTerm(TermHistory *th);
static term_t GetLastLogTerm(Safekeeper *sk);
static void ProcessPropStartPos(WalProposer *wp);
static void SendProposerElected(Safekeeper *sk);
static void StartStreaming(Safekeeper *sk);
static void SendMessageToNode(Safekeeper *sk);
static void BroadcastAppendRequest(WalProposer *wp);
static void HandleActiveState(Safekeeper *sk, uint32 events);
static bool SendAppendRequests(Safekeeper *sk);
static bool RecvAppendResponses(Safekeeper *sk);
static XLogRecPtr CalculateMinFlushLsn(WalProposer *wp);
static XLogRecPtr GetAcknowledgedByQuorumWALPosition(WalProposer *wp);
static void PAMessageSerialize(WalProposer *wp, ProposerAcceptorMessage *msg, StringInfo buf, int proto_version);
static void HandleSafekeeperResponse(WalProposer *wp, Safekeeper *sk);
static bool AsyncRead(Safekeeper *sk, char **buf, int *buf_size);
static bool AsyncReadMessage(Safekeeper *sk, AcceptorProposerMessage *anymsg);
static bool BlockingWrite(Safekeeper *sk, void *msg, size_t msg_size, SafekeeperState success_state);
static bool AsyncWrite(Safekeeper *sk, void *msg, size_t msg_size, SafekeeperState flush_state);
static bool AsyncFlush(Safekeeper *sk);
static int	CompareLsn(const void *a, const void *b);
static char *FormatSafekeeperState(Safekeeper *sk);
static void AssertEventsOkForState(uint32 events, Safekeeper *sk);
static char *FormatEvents(WalProposer *wp, uint32 events);
static void UpdateDonorShmem(WalProposer *wp);
static char *MembershipConfigurationToString(MembershipConfiguration *mconf);
static void MembershipConfigurationCopy(MembershipConfiguration *src, MembershipConfiguration *dst);
static void MembershipConfigurationFree(MembershipConfiguration *mconf);

WalProposer *
WalProposerCreate(WalProposerConfig *config, walproposer_api api)
{
	char	   *host;
	char	   *sep;
	char	   *port;
	WalProposer *wp;

	wp = palloc0(sizeof(WalProposer));
	wp->config = config;
	wp->api = api;
	wp->localTimeLineID = config->pgTimeline;
	wp->state = WPS_COLLECTING_TERMS;
	wp->mconf.generation = INVALID_GENERATION;
	wp->mconf.members.len = 0;
	wp->mconf.new_members.len = 0;

	wp_log(LOG, "neon.safekeepers=%s", wp->config->safekeepers_list);

	/*
	 * If safekeepers list starts with g# parse generation number followed by
	 * :
	 */
	if (strncmp(wp->config->safekeepers_list, "g#", 2) == 0)
	{
		char	   *endptr;

		errno = 0;
		wp->safekeepers_generation = strtoul(wp->config->safekeepers_list + 2, &endptr, 10);
		if (errno != 0)
		{
			wp_log(FATAL, "failed to parse neon.safekeepers generation number: %m");
		}
		if (*endptr != ':')
		{
			wp_log(FATAL, "failed to parse neon.safekeepers: no colon after generation");
		}
		/* Skip past : to the first hostname. */
		host = endptr + 1;
	}
	else
	{
		wp->safekeepers_generation = INVALID_GENERATION;
		host = wp->config->safekeepers_list;
	}
	wp_log(LOG, "safekeepers_generation=%u", wp->safekeepers_generation);

	for (; host != NULL && *host != '\0'; host = sep)
	{
		port = strchr(host, ':');
		if (port == NULL)
		{
			wp_log(FATAL, "port is not specified");
		}
		*port++ = '\0';
		sep = strchr(port, ',');
		if (sep != NULL)
			*sep++ = '\0';
		if (wp->n_safekeepers + 1 >= MAX_SAFEKEEPERS)
		{
			wp_log(FATAL, "too many safekeepers");
		}
		wp->safekeeper[wp->n_safekeepers].host = host;
		wp->safekeeper[wp->n_safekeepers].port = port;
		wp->safekeeper[wp->n_safekeepers].state = SS_OFFLINE;
		wp->safekeeper[wp->n_safekeepers].active_state = SS_ACTIVE_SEND;
		wp->safekeeper[wp->n_safekeepers].wp = wp;
		/* BEGIN_HADRON */
		wp->safekeeper[wp->n_safekeepers].index = wp->n_safekeepers;
		/* END_HADRON */
		{
			Safekeeper *sk = &wp->safekeeper[wp->n_safekeepers];
			int			written = 0;

			written = snprintf((char *) &sk->conninfo, MAXCONNINFO,
							   "%s host=%s port=%s dbname=replication options='-c timeline_id=%s tenant_id=%s'",
							   wp->config->safekeeper_conninfo_options, sk->host, sk->port,
							   wp->config->neon_timeline, wp->config->neon_tenant);
			if (written > MAXCONNINFO || written < 0)
				wp_log(FATAL, "could not create connection string for safekeeper %s:%s", sk->host, sk->port);
		}

		initStringInfo(&wp->safekeeper[wp->n_safekeepers].outbuf);
		wp->safekeeper[wp->n_safekeepers].startStreamingAt = InvalidXLogRecPtr;
		wp->safekeeper[wp->n_safekeepers].streamingAt = InvalidXLogRecPtr;
		wp->n_safekeepers += 1;
	}
	if (wp->n_safekeepers < 1)
	{
		wp_log(FATAL, "safekeepers addresses are not specified");
	}
	wp->quorum = wp->n_safekeepers / 2 + 1;

	if (wp->config->proto_version != 2 && wp->config->proto_version != 3)
		wp_log(FATAL, "unsupported safekeeper protocol version %d", wp->config->proto_version);
	if (wp->safekeepers_generation > INVALID_GENERATION && wp->config->proto_version < 3)
		wp_log(FATAL, "enabling generations requires protocol version 3");
	wp_log(LOG, "using safekeeper protocol version %d", wp->config->proto_version);
	
	/* BEGIN_HADRON */
	wp->api.reset_safekeeper_statuses_for_metrics(wp, wp->n_safekeepers);
	/* END_HADRON */

	/* Fill the greeting package */
	wp->greetRequest.pam.tag = 'g';
	if (!wp->config->neon_tenant)
		wp_log(FATAL, "neon.tenant_id is not provided");
	wp->greetRequest.tenant_id = wp->config->neon_tenant;
	if (!wp->config->neon_timeline)
		wp_log(FATAL, "neon.timeline_id is not provided");
	wp->greetRequest.timeline_id = wp->config->neon_timeline;
	wp->greetRequest.pg_version = PG_VERSION_NUM;
	wp->greetRequest.system_id = wp->config->systemId;
	wp->greetRequest.wal_seg_size = wp->config->wal_segment_size;

	wp->api.init_event_set(wp);

	return wp;
}

void
WalProposerFree(WalProposer *wp)
{
	MembershipConfigurationFree(&wp->mconf);
	for (int i = 0; i < wp->n_safekeepers; i++)
	{
		Safekeeper *sk = &wp->safekeeper[i];

		Assert(sk->outbuf.data != NULL);
		pfree(sk->outbuf.data);
		MembershipConfigurationFree(&sk->greetResponse.mconf);
		if (sk->voteResponse.termHistory.entries)
			pfree(sk->voteResponse.termHistory.entries);
		sk->voteResponse.termHistory.entries = NULL;
	}
	if (wp->propTermHistory.entries != NULL)
		pfree(wp->propTermHistory.entries);
	wp->propTermHistory.entries = NULL;

	pfree(wp);
}

static bool
WalProposerGenerationsEnabled(WalProposer *wp)
{
	return wp->safekeepers_generation != INVALID_GENERATION;
}

/*
 * Create new AppendRequest message and start sending it. This function is
 * called from walsender every time the new WAL is available.
 */
void
WalProposerBroadcast(WalProposer *wp, XLogRecPtr startpos, XLogRecPtr endpos)
{
	Assert(startpos == wp->availableLsn && endpos >= wp->availableLsn);
	wp->availableLsn = endpos;
	BroadcastAppendRequest(wp);
}

/*
 * Advance the WAL proposer state machine, waiting each time for events to occur.
 * Will exit only when latch is set, i.e. new WAL should be pushed from walsender
 * to walproposer.
 */
void
WalProposerPoll(WalProposer *wp)
{
	while (true)
	{
		Safekeeper *sk = NULL;
		int			rc = 0;
		uint32		events = 0;
		TimestampTz now = wp->api.get_current_timestamp(wp);
		long		timeout = TimeToReconnect(wp, now);

		rc = wp->api.wait_event_set(wp, timeout, &sk, &events);

		/* Exit loop if latch is set (we got new WAL) */
		if (rc == 1 && (events & WL_LATCH_SET))
			break;

		/*
		 * If the event contains something that one of our safekeeper states
		 * was waiting for, we'll advance its state.
		 */
		if (rc == 1 && (events & WL_SOCKET_MASK))
		{
			Assert(sk != NULL);
			AdvancePollState(sk, events);
		}

		/*
		 * If the timeout expired, attempt to reconnect to any safekeepers
		 * that we dropped
		 */
		ReconnectSafekeepers(wp);

		if (rc == 0)			/* timeout expired */
		{
			/*
			 * Ensure flushrecptr is set to a recent value. This fixes a case
			 * where we've not been notified of new WAL records when we were
			 * planning on consuming them.
			 */
			if (!wp->config->syncSafekeepers)
			{
				XLogRecPtr	flushed = wp->api.get_flush_rec_ptr(wp);

				if (flushed > wp->availableLsn)
					break;
			}
		}

		now = wp->api.get_current_timestamp(wp);
		/* timeout expired: poll state */
		if (rc == 0 || TimeToReconnect(wp, now) <= 0)
		{
			/*
			 * If no WAL was generated during timeout (and we have already
			 * collected the quorum), then send empty keepalive message
			 */
			if (wp->availableLsn != InvalidXLogRecPtr)
			{
				BroadcastAppendRequest(wp);
			}

			/*
			 * Abandon connection attempts which take too long.
			 */
			now = wp->api.get_current_timestamp(wp);
			for (int i = 0; i < wp->n_safekeepers; i++)
			{
				sk = &wp->safekeeper[i];
				if (TimestampDifferenceExceeds(sk->latestMsgReceivedAt, now,
											   wp->config->safekeeper_connection_timeout))
				{
					wp_log(WARNING, "terminating connection to safekeeper '%s:%s' in '%s' state: no messages received during the last %dms or connection attempt took longer than that",
						   sk->host, sk->port, FormatSafekeeperState(sk), wp->config->safekeeper_connection_timeout);
					ShutdownConnection(sk);
				}
			}
		}
	}
}

void
WalProposerStart(WalProposer *wp)
{

	/* Initiate connections to all safekeeper nodes */
	for (int i = 0; i < wp->n_safekeepers; i++)
	{
		ResetConnection(&wp->safekeeper[i]);
	}

	WalProposerLoop(wp);
}

static void
WalProposerLoop(WalProposer *wp)
{
	while (true)
		WalProposerPoll(wp);
}


/* Shuts down and cleans up the connection for a safekeeper. Sets its state to SS_OFFLINE */
static void
ShutdownConnection(Safekeeper *sk)
{
	sk->state = SS_OFFLINE;
	sk->streamingAt = InvalidXLogRecPtr;

	/* BEGIN_HADRON */
	sk->wp->api.update_safekeeper_status_for_metrics(sk->wp, sk->index, 0);
	/* END_HADRON */

	MembershipConfigurationFree(&sk->greetResponse.mconf);
	if (sk->voteResponse.termHistory.entries)
		pfree(sk->voteResponse.termHistory.entries);
	sk->voteResponse.termHistory.entries = NULL;

	sk->wp->api.conn_finish(sk);
	sk->wp->api.rm_safekeeper_event_set(sk);
}

/*
 * This function is called to establish new connection or to reestablish
 * connection in case of connection failure.
 *
 * On success, sets the state to SS_CONNECTING_WRITE.
 */
static void
ResetConnection(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;

	if (sk->state != SS_OFFLINE)
	{
		ShutdownConnection(sk);
	}

	/*
	 * Try to establish new connection, it will update sk->conn.
	 */
	wp->api.conn_connect_start(sk);

	/*
	 * PQconnectStart won't actually start connecting until we run
	 * PQconnectPoll. Before we do that though, we need to check that it
	 * didn't immediately fail.
	 */
	if (wp->api.conn_status(sk) == WP_CONNECTION_BAD)
	{
		/*---
		 * According to libpq docs:
		 *   "If the result is CONNECTION_BAD, the connection attempt has already failed,
		 *    typically because of invalid connection parameters."
		 * We should report this failure. Do not print the exact `conninfo` as it may
		 * contain e.g. password. The error message should already provide enough information.
		 *
		 * https://www.postgresql.org/docs/devel/libpq-connect.html#LIBPQ-PQCONNECTSTARTPARAMS
		 */
		wp_log(WARNING, "immediate failure to connect with node '%s:%s':\n\terror: %s",
			   sk->host, sk->port, wp->api.conn_error_message(sk));

		/*
		 * Even though the connection failed, we still need to clean up the
		 * object
		 */
		wp->api.conn_finish(sk);
		return;
	}

	/*
	 * The documentation for PQconnectStart states that we should call
	 * PQconnectPoll in a loop until it returns PGRES_POLLING_OK or
	 * PGRES_POLLING_FAILED. The other two possible returns indicate whether
	 * we should wait for reading or writing on the socket. For the first
	 * iteration of the loop, we're expected to wait until the socket becomes
	 * writable.
	 *
	 * The wording of the documentation is a little ambiguous; thankfully
	 * there's an example in the postgres source itself showing this behavior.
	 * (see libpqrcv_connect, defined in
	 * src/backend/replication/libpqwalreceiver/libpqwalreceiver.c)
	 */
	wp_log(LOG, "connecting with node %s:%s", sk->host, sk->port);

	sk->state = SS_CONNECTING_WRITE;
	sk->latestMsgReceivedAt = wp->api.get_current_timestamp(wp);

	wp->api.add_safekeeper_event_set(sk, WL_SOCKET_WRITEABLE);
	return;
}

/*
 * How much milliseconds left till we should attempt reconnection to
 * safekeepers? Returns 0 if it is already high time, -1 if we never reconnect
 * (do we actually need this?).
 */
static long
TimeToReconnect(WalProposer *wp, TimestampTz now)
{
	TimestampTz passed;
	TimestampTz till_reconnect;

	if (wp->config->safekeeper_reconnect_timeout <= 0)
		return -1;

	passed = now - wp->last_reconnect_attempt;
	till_reconnect = wp->config->safekeeper_reconnect_timeout * 1000 - passed;
	if (till_reconnect <= 0)
		return 0;
	return (long) (till_reconnect / 1000);
}

/* If the timeout has expired, attempt to reconnect to all offline safekeepers */
static void
ReconnectSafekeepers(WalProposer *wp)
{
	TimestampTz now = wp->api.get_current_timestamp(wp);

	if (TimeToReconnect(wp, now) == 0)
	{
		wp->last_reconnect_attempt = now;
		for (int i = 0; i < wp->n_safekeepers; i++)
		{
			if (wp->safekeeper[i].state == SS_OFFLINE)
				ResetConnection(&wp->safekeeper[i]);
		}
	}
}

/*
 * Performs the logic for advancing the state machine of the specified safekeeper,
 * given that a certain set of events has occurred.
 */
static void
AdvancePollState(Safekeeper *sk, uint32 events)
{
#ifdef WALPROPOSER_LIB			/* wp_log needs wp in lib build */
	WalProposer *wp = sk->wp;
#endif

	/*
	 * Sanity check. We assume further down that the operations don't block
	 * because the socket is ready.
	 */
	AssertEventsOkForState(events, sk);

	/* Execute the code corresponding to the current state */
	switch (sk->state)
	{
			/*
			 * safekeepers are only taken out of SS_OFFLINE by calls to
			 * ResetConnection
			 */
		case SS_OFFLINE:
			wp_log(FATAL, "unexpected safekeeper %s:%s state advancement: is offline",
				   sk->host, sk->port);
			break;				/* actually unreachable, but prevents
								 * -Wimplicit-fallthrough */

			/*
			 * Both connecting states run the same logic. The only difference
			 * is the events they're expecting
			 */
		case SS_CONNECTING_READ:
		case SS_CONNECTING_WRITE:
			HandleConnectionEvent(sk);
			break;

			/*
			 * Waiting for a successful CopyBoth response.
			 */
		case SS_WAIT_EXEC_RESULT:
			RecvStartWALPushResult(sk);
			break;

			/*
			 * Finish handshake comms: receive information about the
			 * safekeeper.
			 */
		case SS_HANDSHAKE_RECV:
			RecvAcceptorGreeting(sk);
			break;

			/*
			 * Voting is an idle state - we don't expect any events to
			 * trigger. Refer to the execution of SS_HANDSHAKE_RECV to see how
			 * nodes are transferred from SS_VOTING to sending actual vote
			 * requests.
			 */
		case SS_WAIT_VOTING:
			wp_log(WARNING, "EOF from node %s:%s in %s state", sk->host,
				   sk->port, FormatSafekeeperState(sk));
			ResetConnection(sk);
			return;

			/* Read the safekeeper response for our candidate */
		case SS_WAIT_VERDICT:
			RecvVoteResponse(sk);
			break;

			/* Flush proposer announcement message */
		case SS_SEND_ELECTED_FLUSH:

			/*
			 * AsyncFlush ensures we only move on to SS_ACTIVE once the flush
			 * completes. If we still have more to do, we'll wait until the
			 * next poll comes along.
			 */
			if (!AsyncFlush(sk))
				return;

			/* flush is done, event set and state will be updated later */
			StartStreaming(sk);
			break;

			/*
			 * Idle state for waiting votes from quorum.
			 */
		case SS_WAIT_ELECTED:
			wp_log(WARNING, "EOF from node %s:%s in %s state", sk->host,
				   sk->port, FormatSafekeeperState(sk));
			ResetConnection(sk);
			return;

			/*
			 * Active state is used for streaming WAL and receiving feedback.
			 */
		case SS_ACTIVE:
			HandleActiveState(sk, events);
			break;
	}
}

static void
HandleConnectionEvent(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;
	WalProposerConnectPollStatusType result = wp->api.conn_connect_poll(sk);

	/* The new set of events we'll wait on, after updating */
	uint32		new_events = WL_NO_EVENTS;

	switch (result)
	{
		case WP_CONN_POLLING_OK:
			wp_log(LOG, "connected with node %s:%s", sk->host,
				   sk->port);
			sk->latestMsgReceivedAt = wp->api.get_current_timestamp(wp);

			/*
			 * We have to pick some event to update event set. We'll
			 * eventually need the socket to be readable, so we go with that.
			 */
			new_events = WL_SOCKET_READABLE;
			break;

			/*
			 * If we need to poll to finish connecting, continue doing that
			 */
		case WP_CONN_POLLING_READING:
			sk->state = SS_CONNECTING_READ;
			new_events = WL_SOCKET_READABLE;
			break;
		case WP_CONN_POLLING_WRITING:
			sk->state = SS_CONNECTING_WRITE;
			new_events = WL_SOCKET_WRITEABLE;
			break;

		case WP_CONN_POLLING_FAILED:
			wp_log(WARNING, "failed to connect to node '%s:%s': %s",
				   sk->host, sk->port, wp->api.conn_error_message(sk));

			/*
			 * If connecting failed, we don't want to restart the connection
			 * because that might run us into a loop. Instead, shut it down --
			 * it'll naturally restart at a slower interval on calls to
			 * ReconnectSafekeepers.
			 */
			ShutdownConnection(sk);
			return;
	}

	/*
	 * Because PQconnectPoll can change the socket, we have to un-register the
	 * old event and re-register an event on the new socket.
	 */
	wp->api.rm_safekeeper_event_set(sk);
	wp->api.add_safekeeper_event_set(sk, new_events);

	/* If we successfully connected, send START_WAL_PUSH query */
	if (result == WP_CONN_POLLING_OK)
		SendStartWALPush(sk);
}

/*
 * Send "START_WAL_PUSH" message as an empty query to the safekeeper. Performs
 * a blocking send, then immediately moves to SS_WAIT_EXEC_RESULT. If something
 * goes wrong, change state to SS_OFFLINE and shutdown the connection.
 */
static void
SendStartWALPush(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;

	/* Forbid implicit timeline creation if generations are enabled. */
	char	   *allow_timeline_creation = WalProposerGenerationsEnabled(wp) ? "false" : "true";
#define CMD_LEN 512
	char		cmd[CMD_LEN];


	snprintf(cmd, CMD_LEN, "START_WAL_PUSH (proto_version '%d', allow_timeline_creation '%s')", wp->config->proto_version, allow_timeline_creation);
	if (!wp->api.conn_send_query(sk, cmd))
	{
		wp_log(WARNING, "failed to send '%s' query to safekeeper %s:%s: %s",
			   cmd, sk->host, sk->port, wp->api.conn_error_message(sk));
		ShutdownConnection(sk);
		return;
	}
	sk->state = SS_WAIT_EXEC_RESULT;
	wp->api.update_event_set(sk, WL_SOCKET_READABLE);
}

static void
RecvStartWALPushResult(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;

	switch (wp->api.conn_get_query_result(sk))
	{
			/*
			 * Successful result, move on to starting the handshake
			 */
		case WP_EXEC_SUCCESS_COPYBOTH:

			SendProposerGreeting(sk);
			break;

			/*
			 * Needs repeated calls to finish. Wait until the socket is
			 * readable
			 */
		case WP_EXEC_NEEDS_INPUT:

			/*
			 * SS_WAIT_EXEC_RESULT is always reached through an event, so we
			 * don't need to update the event set
			 */
			break;

		case WP_EXEC_FAILED:
			wp_log(WARNING, "failed to send query to safekeeper %s:%s: %s",
				   sk->host, sk->port, wp->api.conn_error_message(sk));
			ShutdownConnection(sk);
			return;

			/*
			 * Unexpected result -- funamdentally an error, but we want to
			 * produce a custom message, rather than a generic "something went
			 * wrong"
			 */
		case WP_EXEC_UNEXPECTED_SUCCESS:
			wp_log(WARNING, "received bad response from safekeeper %s:%s query execution",
				   sk->host, sk->port);
			ShutdownConnection(sk);
			return;
	}
}

/*
 * Start handshake: first of all send information about the
 * walproposer. After sending, we wait on SS_HANDSHAKE_RECV for
 * a response to finish the handshake.
 */
static void
SendProposerGreeting(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;
	char	   *mconf_toml = MembershipConfigurationToString(&wp->greetRequest.mconf);

	wp_log(LOG, "sending ProposerGreeting to safekeeper %s:%s with mconf = %s", sk->host, sk->port, mconf_toml);
	pfree(mconf_toml);

	PAMessageSerialize(wp, (ProposerAcceptorMessage *) &wp->greetRequest,
					   &sk->outbuf, wp->config->proto_version);

	/*
	 * On failure, logging & resetting the connection is handled. We just need
	 * to handle the control flow.
	 */
	BlockingWrite(sk, sk->outbuf.data, sk->outbuf.len, SS_HANDSHAKE_RECV);
}

/*
 * Assuming `sk` sent its node id, find such member(s) in wp->mconf and set ptr in
 * members_safekeepers & new_members_safekeepers to sk.
 */
static void
UpdateMemberSafekeeperPtr(WalProposer *wp, Safekeeper *sk)
{
	/* members_safekeepers etc are fixed size, sanity check mconf size */
	if (wp->mconf.members.len > MAX_SAFEKEEPERS)
		wp_log(FATAL, "too many members %d in mconf", wp->mconf.members.len);
	if (wp->mconf.new_members.len > MAX_SAFEKEEPERS)
		wp_log(FATAL, "too many new_members %d in mconf", wp->mconf.new_members.len);

	/* node id is not known until greeting is received */
	if (sk->state < SS_WAIT_VOTING)
		return;

	/* 0 is assumed to be invalid node id, should never happen */
	if (sk->greetResponse.nodeId == 0)
	{
		wp_log(WARNING, "safekeeper %s:%s sent zero node id", sk->host, sk->port);
		return;
	}

	for (uint32 i = 0; i < wp->mconf.members.len; i++)
	{
		SafekeeperId *sk_id = &wp->mconf.members.m[i];

		if (sk_id->node_id == sk->greetResponse.nodeId)
		{
			/*
			 * If mconf or list of safekeepers to connect to changed (the
			 * latter always currently goes through restart though),
			 * ResetMemberSafekeeperPtrs is expected to be called before
			 * UpdateMemberSafekeeperPtr. So, other value suggests that we are
			 * connected to the same sk under different host name, complain
			 * about that.
			 */
			if (wp->members_safekeepers[i] != NULL && wp->members_safekeepers[i] != sk)
			{
				wp_log(WARNING, "safekeeper {id = %lu, ep = %s:%u } in members[%u] is already mapped to connection slot %lu",
					   sk_id->node_id, sk_id->host, sk_id->port, i, wp->members_safekeepers[i] - wp->safekeeper);
			}
			wp_log(LOG, "safekeeper {id = %lu, ep = %s:%u } in members[%u] mapped to connection slot %lu",
				   sk_id->node_id, sk_id->host, sk_id->port, i, sk - wp->safekeeper);
			wp->members_safekeepers[i] = sk;
		}
	}
	/* repeat for new_members */
	for (uint32 i = 0; i < wp->mconf.new_members.len; i++)
	{
		SafekeeperId *sk_id = &wp->mconf.new_members.m[i];

		if (sk_id->node_id == sk->greetResponse.nodeId)
		{
			if (wp->new_members_safekeepers[i] != NULL && wp->new_members_safekeepers[i] != sk)
			{
				wp_log(WARNING, "safekeeper {id = %lu, ep = %s:%u } in new_members[%u] is already mapped to connection slot %lu",
					   sk_id->node_id, sk_id->host, sk_id->port, i, wp->new_members_safekeepers[i] - wp->safekeeper);
			}
			wp_log(LOG, "safekeeper {id = %lu, ep = %s:%u } in new_members[%u] mapped to connection slot %lu",
				   sk_id->node_id, sk_id->host, sk_id->port, i, sk - wp->safekeeper);
			wp->new_members_safekeepers[i] = sk;
		}
	}
}

/*
 * Reset wp->members_safekeepers & new_members_safekeepers and refill them.
 * Called after wp changes mconf.
 */
static void
ResetMemberSafekeeperPtrs(WalProposer *wp)
{
	memset(&wp->members_safekeepers, 0, sizeof(Safekeeper *) * MAX_SAFEKEEPERS);
	memset(&wp->new_members_safekeepers, 0, sizeof(Safekeeper *) * MAX_SAFEKEEPERS);
	for (int i = 0; i < wp->n_safekeepers; i++)
	{
		if (wp->safekeeper[i].state >= SS_WAIT_VOTING)
			UpdateMemberSafekeeperPtr(wp, &wp->safekeeper[i]);
	}
}

static uint32
MsetQuorum(MemberSet *mset)
{
	Assert(mset->len > 0);
	return mset->len / 2 + 1;
}

/* Does n forms quorum in mset? */
static bool
MsetHasQuorum(MemberSet *mset, uint32 n)
{
	return n >= MsetQuorum(mset);
}

/*
 * TermsCollected helper for a single member set `mset`.
 *
 * `msk` is the member -> safekeeper mapping for mset, i.e. members_safekeepers
 * or new_members_safekeepers.
 */
static bool
TermsCollectedMset(WalProposer *wp, MemberSet *mset, Safekeeper **msk, StringInfo s)
{
	uint32		n_greeted = 0;

	for (uint32 i = 0; i < mset->len; i++)
	{
		Safekeeper *sk = msk[i];

		if (sk != NULL && sk->state == SS_WAIT_VOTING)
		{
			if (n_greeted > 0)
				appendStringInfoString(s, ", ");
			appendStringInfo(s, "{id = %lu, ep = %s:%s}", sk->greetResponse.nodeId, sk->host, sk->port);
			n_greeted++;
		}
	}
	appendStringInfo(s, ", %u/%u total", n_greeted, mset->len);
	return MsetHasQuorum(mset, n_greeted);
}

/*
 * Have we received greeting from enough (quorum) safekeepers to start voting?
 */
static bool
TermsCollected(WalProposer *wp)
{
	StringInfoData s;			/* str for logging */
	bool		collected = false;

	/* legacy: generations disabled */
	if (!WalProposerGenerationsEnabled(wp) && wp->mconf.generation == INVALID_GENERATION)
	{
		collected = wp->n_connected >= wp->quorum;
		if (collected)
		{
			wp->propTerm++;
			wp_log(LOG, "walproposer connected to quorum (%d) safekeepers, propTerm=" INT64_FORMAT ", starting voting", wp->quorum, wp->propTerm);
		}
		return collected;
	}

	/*
	 * With generations enabled, we start campaign only when 1) some mconf is
	 * actually received 2) we have greetings from majority of members as well
	 * as from majority of new_members if it exists.
	 */
	if (wp->mconf.generation == INVALID_GENERATION)
		return false;

	initStringInfo(&s);
	appendStringInfoString(&s, "mset greeters: ");
	if (!TermsCollectedMset(wp, &wp->mconf.members, wp->members_safekeepers, &s))
		goto res;
	if (wp->mconf.new_members.len > 0)
	{
		appendStringInfoString(&s, ", new_mset greeters: ");
		if (!TermsCollectedMset(wp, &wp->mconf.new_members, wp->new_members_safekeepers, &s))
			goto res;
	}
	wp->propTerm++;
	wp_log(LOG, "walproposer connected to quorum of safekeepers: %s, propTerm=" INT64_FORMAT ", starting voting", s.data, wp->propTerm);
	collected = true;

res:
	pfree(s.data);
	return collected;
}

static void
RecvAcceptorGreeting(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;
	char	   *mconf_toml;

	/*
	 * If our reading doesn't immediately succeed, any necessary error
	 * handling or state setting is taken care of. We can leave any other work
	 * until later.
	 */
	sk->greetResponse.apm.tag = 'g';
	if (!AsyncReadMessage(sk, (AcceptorProposerMessage *) &sk->greetResponse))
		return;

	mconf_toml = MembershipConfigurationToString(&sk->greetResponse.mconf);
	wp_log(LOG, "received AcceptorGreeting from safekeeper %s:%s, node_id = %lu, mconf = %s, term=" UINT64_FORMAT,
		   sk->host, sk->port, sk->greetResponse.nodeId, mconf_toml, sk->greetResponse.term);
	pfree(mconf_toml);

	/*
	 * Adopt mconf of safekeepers if it is higher.
	 */
	if (sk->greetResponse.mconf.generation > wp->mconf.generation)
	{
		/* sanity check before adopting, should never happen */
		if (sk->greetResponse.mconf.members.len == 0)
		{
			wp_log(FATAL, "mconf %u has zero members", sk->greetResponse.mconf.generation);
		}

		/*
		 * If we at least started campaign, restart wp to get elected in the
		 * new mconf. Note: in principle once wp is already elected
		 * re-election is not required, but being conservative here is not
		 * bad.
		 *
		 * TODO: put mconf to shmem to immediately pick it up on start,
		 * otherwise if some safekeeper(s) misses latest mconf and gets
		 * connected the first, it may cause redundant restarts here.
		 *
		 * More generally, it would be nice to restart walproposer (wiping
		 * election state) without restarting the process. In particular, that
		 * would allow sync-safekeepers not to die here if it intersected with
		 * sk migration (as well as remove 1s delay).
		 *
		 * Note that assign_neon_safekeepers also currently restarts the
		 * process, so during normal migration walproposer may restart twice.
		 */
		if (wp->state >= WPS_CAMPAIGN)
		{
			wp_log(FATAL, "restarting to adopt mconf generation %d", sk->greetResponse.mconf.generation);
		}
		MembershipConfigurationFree(&wp->mconf);
		MembershipConfigurationCopy(&sk->greetResponse.mconf, &wp->mconf);
		ResetMemberSafekeeperPtrs(wp);
		/* full conf was just logged above */
		wp_log(LOG, "changed mconf to generation %u", wp->mconf.generation);
	}

	/* Protocol is all good, move to voting. */
	sk->state = SS_WAIT_VOTING;

	/* In greeting safekeeper sent its id; update mappings accordingly. */
	UpdateMemberSafekeeperPtr(wp, sk);

	/*
	 * Note: it would be better to track the counter on per safekeeper basis,
	 * but at worst walproposer would restart with 'term rejected', so leave
	 * as is for now.
	 */
	++wp->n_connected;
	if (wp->state == WPS_COLLECTING_TERMS)
	{
		/* We're still collecting terms from the majority. */
		wp->propTerm = Max(sk->greetResponse.term, wp->propTerm);

		/* Quorum is acquired, prepare the vote request. */
		if (TermsCollected(wp))
		{
			wp->state = WPS_CAMPAIGN;
			wp->voteRequest.pam.tag = 'v';
			wp->voteRequest.generation = wp->mconf.generation;
			wp->voteRequest.term = wp->propTerm;
		}
	}
	else if (sk->greetResponse.term > wp->propTerm)
	{
		/* Another compute with higher term is running. */
		wp_log(FATAL, "WAL acceptor %s:%s with term " INT64_FORMAT " rejects our connection request with term " INT64_FORMAT "",
			   sk->host, sk->port,
			   sk->greetResponse.term, wp->propTerm);
	}

	/*
	 * If we have quorum, start (or just send vote request to newly connected
	 * node) election, otherwise wait until we have more greetings.
	 */
	if (wp->state == WPS_COLLECTING_TERMS)
	{
		/*
		 * SS_VOTING is an idle state; read-ready indicates the connection
		 * closed.
		 */
		wp->api.update_event_set(sk, WL_SOCKET_READABLE);
	}
	else
	{
		/*
		 * Now send voting request to the cohort and wait responses
		 */
		for (int j = 0; j < wp->n_safekeepers; j++)
		{
			if (wp->safekeeper[j].state == SS_WAIT_VOTING)
				SendVoteRequest(&wp->safekeeper[j]);
		}
	}
}

static void
SendVoteRequest(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;

	PAMessageSerialize(wp, (ProposerAcceptorMessage *) &wp->voteRequest,
					   &sk->outbuf, wp->config->proto_version);

	/* We have quorum for voting, send our vote request */
	wp_log(LOG, "requesting vote from sk {id = %lu, ep = %s:%s} for generation %u term " UINT64_FORMAT,
		   sk->greetResponse.nodeId, sk->host, sk->port, wp->voteRequest.generation, wp->voteRequest.term);
	/* On failure, logging & resetting is handled */
	BlockingWrite(sk, sk->outbuf.data, sk->outbuf.len, SS_WAIT_VERDICT);
	/* If successful, wait for read-ready with SS_WAIT_VERDICT */
}

static void
RecvVoteResponse(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;

	Assert(wp->state >= WPS_CAMPAIGN);

	sk->voteResponse.apm.tag = 'v';
	if (!AsyncReadMessage(sk, (AcceptorProposerMessage *) &sk->voteResponse))
		return;

	wp_log(LOG,
		   "got VoteResponse from sk {id = %lu, ep = %s:%s}, generation=%u, term=%lu, voteGiven=%u, last_log_term=" UINT64_FORMAT ", flushLsn=%X/%X, truncateLsn=%X/%X",
		   sk->greetResponse.nodeId, sk->host, sk->port, sk->voteResponse.generation, sk->voteResponse.term,
		   sk->voteResponse.voteGiven,
		   GetHighestTerm(&sk->voteResponse.termHistory),
		   LSN_FORMAT_ARGS(sk->voteResponse.flushLsn),
		   LSN_FORMAT_ARGS(sk->voteResponse.truncateLsn));

	/*
	 * In case of acceptor rejecting our vote, bail out, but only if either it
	 * already lives in strictly higher term (concurrent compute spotted) or
	 * we are not elected yet and thus need the vote.
	 */
	if ((!sk->voteResponse.voteGiven) &&
		(sk->voteResponse.term > wp->propTerm || wp->state == WPS_CAMPAIGN))
	{
		wp_log(FATAL, "WAL acceptor %s:%s with term " INT64_FORMAT " rejects our connection request with term " INT64_FORMAT "",
			   sk->host, sk->port,
			   sk->voteResponse.term, wp->propTerm);
	}
	Assert(sk->voteResponse.term == wp->propTerm);

	/* ready for elected message */
	sk->state = SS_WAIT_ELECTED;

	/* Are we already elected? */
	if (wp->state == WPS_CAMPAIGN)
	{
		/* no; check if this vote makes us elected */
		if (VotesCollected(wp))
		{
			wp->state = WPS_ELECTED;
			HandleElectedProposer(wp);
		}
		else
		{
			/* can't do much yet, no quorum */
			return;
		}
	}
	else
	{
		Assert(wp->state == WPS_ELECTED);
		/* send elected only to this sk */
		SendProposerElected(sk);
	}
}

/*
 * VotesCollected helper for a single member set `mset`.
 *
 * `msk` is the member -> safekeeper mapping for mset, i.e. members_safekeepers
 * or new_members_safekeepers.
 */
static bool
VotesCollectedMset(WalProposer *wp, MemberSet *mset, Safekeeper **msk, StringInfo s)
{
	uint32		n_votes = 0;

	for (uint32 i = 0; i < mset->len; i++)
	{
		Safekeeper *sk = msk[i];

		if (sk != NULL && sk->state == SS_WAIT_ELECTED)
		{
			Assert(sk->voteResponse.voteGiven);

			/*
			 * Find the highest vote. NULL check is for the legacy case where
			 * safekeeper might be not initialized with LSN at all and return
			 * 0 LSN in the vote response; we still want to set donor to
			 * something in this case.
			 */
			if (GetLastLogTerm(sk) > wp->donorLastLogTerm ||
				(GetLastLogTerm(sk) == wp->donorLastLogTerm &&
				 sk->voteResponse.flushLsn > wp->propTermStartLsn) ||
				wp->donor == NULL)
			{
				wp->donorLastLogTerm = GetLastLogTerm(sk);
				wp->propTermStartLsn = sk->voteResponse.flushLsn;
				wp->donor = sk;
			}
			wp->truncateLsn = Max(sk->voteResponse.truncateLsn, wp->truncateLsn);

			if (n_votes > 0)
				appendStringInfoString(s, ", ");
			appendStringInfo(s, "{id = %lu, ep = %s:%s}", sk->greetResponse.nodeId, sk->host, sk->port);
			n_votes++;
		}
	}
	appendStringInfo(s, ", %u/%u total", n_votes, mset->len);
	return MsetHasQuorum(mset, n_votes);
}


/*
 * Checks if enough votes has been collected to get elected and if that's the
 * case finds the highest vote, setting donor, donorLastLogTerm,
 * propTermStartLsn fields. Also sets truncateLsn.
 */
static bool
VotesCollected(WalProposer *wp)
{
	StringInfoData s;			/* str for logging */
	bool		collected = false;

	/* assumed to be called only when not elected yet */
	Assert(wp->state == WPS_CAMPAIGN);

	wp->propTermStartLsn = InvalidXLogRecPtr;
	wp->donorLastLogTerm = 0;
	wp->truncateLsn = InvalidXLogRecPtr;

	/* legacy: generations disabled */
	if (!WalProposerGenerationsEnabled(wp) && wp->mconf.generation == INVALID_GENERATION)
	{
		int			n_ready = 0;

		for (int i = 0; i < wp->n_safekeepers; i++)
		{
			if (wp->safekeeper[i].state == SS_WAIT_ELECTED)
			{
				n_ready++;

				if (GetLastLogTerm(&wp->safekeeper[i]) > wp->donorLastLogTerm ||
					(GetLastLogTerm(&wp->safekeeper[i]) == wp->donorLastLogTerm &&
					 wp->safekeeper[i].voteResponse.flushLsn > wp->propTermStartLsn) ||
					wp->donor == NULL)
				{
					wp->donorLastLogTerm = GetLastLogTerm(&wp->safekeeper[i]);
					wp->propTermStartLsn = wp->safekeeper[i].voteResponse.flushLsn;
					wp->donor = &wp->safekeeper[i];
				}
				wp->truncateLsn = Max(wp->safekeeper[i].voteResponse.truncateLsn, wp->truncateLsn);
			}
		}
		collected = n_ready >= wp->quorum;
		if (collected)
		{
			wp_log(LOG, "walproposer elected with %d/%d votes", n_ready, wp->n_safekeepers);
		}
		return collected;
	}

	/*
	 * if generations are enabled we're expected to get to voting only when
	 * mconf is established.
	 */
	Assert(wp->mconf.generation != INVALID_GENERATION);

	/*
	 * We must get votes from both msets if both are present.
	 */
	initStringInfo(&s);
	appendStringInfoString(&s, "mset voters: ");
	if (!VotesCollectedMset(wp, &wp->mconf.members, wp->members_safekeepers, &s))
		goto res;
	if (wp->mconf.new_members.len > 0)
	{
		appendStringInfoString(&s, ", new_mset voters: ");
		if (!VotesCollectedMset(wp, &wp->mconf.new_members, wp->new_members_safekeepers, &s))
			goto res;
	}
	wp_log(LOG, "walproposer elected, %s", s.data);
	collected = true;

res:
	pfree(s.data);
	return collected;
}

/*
 * Called once a majority of acceptors have voted for us and current proposer
 * has been elected.
 *
 * Sends ProposerElected message to all acceptors in SS_WAIT_ELECTED state and starts
 * replication from walsender.
 */
static void
HandleElectedProposer(WalProposer *wp)
{
	ProcessPropStartPos(wp);
	Assert(wp->propTermStartLsn != InvalidXLogRecPtr);

	/*
	 * Synchronously download WAL from the most advanced safekeeper. We do
	 * that only for logical replication (and switching logical walsenders to
	 * neon_walreader is a todo.)
	 */
	if (!wp->api.recovery_download(wp, wp->donor))
	{
		wp_log(FATAL, "failed to download WAL for logical replicaiton");
	}

	if (wp->truncateLsn == wp->propTermStartLsn && wp->config->syncSafekeepers)
	{
		/* Sync is not needed: just exit */
		wp->api.finish_sync_safekeepers(wp, wp->propTermStartLsn);
		/* unreachable */
	}

	for (int i = 0; i < wp->n_safekeepers; i++)
	{
		if (wp->safekeeper[i].state == SS_WAIT_ELECTED)
			SendProposerElected(&wp->safekeeper[i]);
	}

	/*
	 * The proposer has been elected, and there will be no quorum waiting
	 * after this point. There will be no safekeeper with state
	 * SS_WAIT_ELECTED also, because that state is used only for quorum
	 * waiting.
	 */

	if (wp->config->syncSafekeepers)
	{
		/*
		 * Send empty message to enforce receiving feedback even from nodes
		 * who are fully recovered; this is required to learn they switched
		 * epoch which finishes sync-safeekepers who doesn't generate any real
		 * new records. Will go away once we switch to async acks.
		 */
		BroadcastAppendRequest(wp);

		/* keep polling until all safekeepers are synced */
		return;
	}

	wp->api.start_streaming(wp, wp->propTermStartLsn);
	/* Should not return here */
}

/* latest term in TermHistory, or 0 is there is no entries */
static term_t
GetHighestTerm(TermHistory *th)
{
	return th->n_entries > 0 ? th->entries[th->n_entries - 1].term : 0;
}

/* safekeeper's epoch is the term of the highest entry in the log */
static term_t
GetLastLogTerm(Safekeeper *sk)
{
	return GetHighestTerm(&sk->voteResponse.termHistory);
}

/* If LSN points to the page header, skip it */
static XLogRecPtr
SkipXLogPageHeader(WalProposer *wp, XLogRecPtr lsn)
{
	if (XLogSegmentOffset(lsn, wp->config->wal_segment_size) == 0)
	{
		lsn += SizeOfXLogLongPHD;
	}
	else if (lsn % XLOG_BLCKSZ == 0)
	{
		lsn += SizeOfXLogShortPHD;
	}
	return lsn;
}

/*
 * Called after quorum gave votes and proposer starting position (highest vote
 * term + flush LSN) -- is determined (VotesCollected true), this function
 * adopts it: pushes LSN to shmem, sets wp term history, verifies that the
 * basebackup matches.
 */
static void
ProcessPropStartPos(WalProposer *wp)
{
	TermHistory *dth;
	WalproposerShmemState *walprop_shared;

	/* must have collected votes */
	Assert(wp->state == WPS_ELECTED);

	/*
	 * If propTermStartLsn is 0, it means flushLsn is 0 everywhere, we are
	 * bootstrapping and nothing was committed yet. Start streaming from the
	 * basebackup LSN then.
	 *
	 * In case of sync-safekeepers just exit: proceeding is not only pointless
	 * but harmful, because we'd give safekeepers term history starting with
	 * 0/0. These hacks will go away once we disable implicit timeline
	 * creation on safekeepers and create it with non zero LSN from the start.
	 */
	if (wp->propTermStartLsn == InvalidXLogRecPtr)
	{
		if (!wp->config->syncSafekeepers)
		{
			wp->propTermStartLsn = wp->truncateLsn = wp->api.get_redo_start_lsn(wp);
			wp_log(LOG, "bumped epochStartLsn to the first record %X/%X", LSN_FORMAT_ARGS(wp->propTermStartLsn));
		}
		else
		{
			wp_log(LOG, "elected with zero propTermStartLsn in sync-safekeepers, exiting");
			wp->api.finish_sync_safekeepers(wp, wp->propTermStartLsn);
		}
	}
	pg_atomic_write_u64(&wp->api.get_shmem_state(wp)->propEpochStartLsn, wp->propTermStartLsn);

	Assert(wp->truncateLsn != InvalidXLogRecPtr || wp->config->syncSafekeepers);

	/*
	 * We will be generating WAL since propTermStartLsn, so we should set
	 * availableLsn to mark this LSN as the latest available position.
	 */
	wp->availableLsn = wp->propTermStartLsn;

	/*
	 * Proposer's term history is the donor's + its own entry.
	 */
	dth = &wp->donor->voteResponse.termHistory;
	wp->propTermHistory.n_entries = dth->n_entries + 1;
	wp->propTermHistory.entries = palloc(sizeof(TermSwitchEntry) * wp->propTermHistory.n_entries);
	if (dth->n_entries > 0)
		memcpy(wp->propTermHistory.entries, dth->entries, sizeof(TermSwitchEntry) * dth->n_entries);
	wp->propTermHistory.entries[wp->propTermHistory.n_entries - 1].term = wp->propTerm;
	wp->propTermHistory.entries[wp->propTermHistory.n_entries - 1].lsn = wp->propTermStartLsn;

	wp_log(LOG, "walproposer elected in term " UINT64_FORMAT ", epochStartLsn %X/%X, donor %s:%s, truncate_lsn %X/%X",
		   wp->propTerm,
		   LSN_FORMAT_ARGS(wp->propTermStartLsn),
		   wp->donor->host, wp->donor->port,
		   LSN_FORMAT_ARGS(wp->truncateLsn));

	/*
	 * Ensure the basebackup we are running (at RedoStartLsn) matches LSN
	 * since which we are going to write according to the consensus. If not,
	 * we must bail out, as clog and other non rel data is inconsistent.
	 */
	walprop_shared = wp->api.get_shmem_state(wp);
	if (!wp->config->syncSafekeepers && !walprop_shared->replica_promote)
	{
		/*
		 * Basebackup LSN always points to the beginning of the record (not
		 * the page), as StartupXLOG most probably wants it this way.
		 * Safekeepers don't skip header as they need continious stream of
		 * data, so correct LSN for comparison.
		 */
		if (SkipXLogPageHeader(wp, wp->propTermStartLsn) != wp->api.get_redo_start_lsn(wp))
		{
			/*
			 * However, allow to proceed if last_log_term on the node which
			 * gave the highest vote (i.e. point where we are going to start
			 * writing) actually had been won by me; plain restart of
			 * walproposer not intervened by concurrent compute which wrote
			 * WAL is ok.
			 *
			 * This avoids compute crash after manual term_bump.
			 */
			if (!((dth->n_entries >= 1) && (dth->entries[dth->n_entries - 1].term ==
											pg_atomic_read_u64(&walprop_shared->mineLastElectedTerm))))
			{
				/*
				 * Panic to restart PG as we need to retake basebackup.
				 * However, don't dump core as this is kinda expected
				 * scenario.
				 */
				disable_core_dump();
				wp_log(PANIC,
					   "collected propTermStartLsn %X/%X, but basebackup LSN %X/%X",
					   LSN_FORMAT_ARGS(wp->propTermStartLsn),
					   LSN_FORMAT_ARGS(wp->api.get_redo_start_lsn(wp)));
			}
		}
	}
	pg_atomic_write_u64(&walprop_shared->mineLastElectedTerm, wp->propTerm);
}

/*
 * Determine for sk the starting streaming point and send it message
 * 1) Announcing we are elected proposer (which immediately advances epoch if
 *    safekeeper is synced, being important for sync-safekeepers)
 * 2) Communicating starting streaming point -- safekeeper must truncate its WAL
 *    beyond it -- and history of term switching.
 *
 * Sets sk->startStreamingAt.
 */
static void
SendProposerElected(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;
	ProposerElected msg;
	TermHistory *th;
	term_t		lastCommonTerm;
	int			idx;

	/* Now that we are ready to send it's a good moment to create WAL reader */
	wp->api.wal_reader_allocate(sk);

	/*
	 * Determine start LSN by comparing safekeeper's log term switch history
	 * and proposer's, searching for the divergence point.
	 *
	 * Note: there is a vanishingly small chance of no common point even if
	 * there is some WAL on safekeeper, if immediately after bootstrap compute
	 * wrote some WAL on single sk and died; we stream since the beginning
	 * then.
	 */
	th = &sk->voteResponse.termHistory;

	/* We must start somewhere. */
	Assert(wp->propTermHistory.n_entries >= 1);

	for (idx = 0; idx < Min(wp->propTermHistory.n_entries, th->n_entries); idx++)
	{
		if (wp->propTermHistory.entries[idx].term != th->entries[idx].term)
			break;
		/* term must begin everywhere at the same point */
		Assert(wp->propTermHistory.entries[idx].lsn == th->entries[idx].lsn);
	}
	idx--;						/* step back to the last common term */
	if (idx < 0)
	{
		/* safekeeper is empty or no common point, start from the beginning */
		sk->startStreamingAt = wp->propTermHistory.entries[0].lsn;
		wp_log(LOG, "no common point with sk %s:%s, streaming since first term at %X/%X, termHistory.n_entries=%u",
			   sk->host, sk->port, LSN_FORMAT_ARGS(sk->startStreamingAt), wp->propTermHistory.n_entries);
	}
	else
	{
		/*
		 * End of (common) term is the start of the next except it is the last
		 * one; there it is flush_lsn in case of safekeeper or, in case of
		 * proposer, LSN it is currently writing, but then we just pick
		 * safekeeper pos as it obviously can't be higher.
		 */
		if (wp->propTermHistory.entries[idx].term == wp->propTerm)
		{
			sk->startStreamingAt = sk->voteResponse.flushLsn;
		}
		else
		{
			XLogRecPtr	propEndLsn = wp->propTermHistory.entries[idx + 1].lsn;
			XLogRecPtr	skEndLsn = (idx + 1 < th->n_entries ? th->entries[idx + 1].lsn : sk->voteResponse.flushLsn);

			sk->startStreamingAt = Min(propEndLsn, skEndLsn);
		}
	}

	Assert(sk->startStreamingAt <= wp->availableLsn);

	msg.apm.tag = 'e';
	msg.generation = wp->mconf.generation;
	msg.term = wp->propTerm;
	msg.startStreamingAt = sk->startStreamingAt;
	msg.termHistory = &wp->propTermHistory;

	lastCommonTerm = idx >= 0 ? wp->propTermHistory.entries[idx].term : 0;
	wp_log(LOG,
		   "sending elected msg to node " UINT64_FORMAT " generation=%u term=" UINT64_FORMAT ", startStreamingAt=%X/%X (lastCommonTerm=" UINT64_FORMAT "), termHistory.n_entries=%u to %s:%s",
		   sk->greetResponse.nodeId, msg.generation, msg.term, LSN_FORMAT_ARGS(msg.startStreamingAt),
		   lastCommonTerm, msg.termHistory->n_entries, sk->host, sk->port);

	PAMessageSerialize(wp, (ProposerAcceptorMessage *) &msg, &sk->outbuf, wp->config->proto_version);
	if (!AsyncWrite(sk, sk->outbuf.data, sk->outbuf.len, SS_SEND_ELECTED_FLUSH))
		return;

	StartStreaming(sk);
}

/*
 * Start streaming to safekeeper sk, always updates state to SS_ACTIVE and sets
 * correct event set.
 */
static void
StartStreaming(Safekeeper *sk)
{
	/*
	 * This is the only entrypoint to state SS_ACTIVE. It's executed exactly
	 * once for a connection.
	 */
	sk->state = SS_ACTIVE;
	sk->active_state = SS_ACTIVE_SEND;
	sk->streamingAt = sk->startStreamingAt;

	/* BEGIN_HADRON */
	sk->wp->api.update_safekeeper_status_for_metrics(sk->wp, sk->index, 1);
	/* END_HADRON */

	/*
	 * Donors can only be in SS_ACTIVE state, so we potentially update the
	 * donor when we switch one to SS_ACTIVE.
	 */
	UpdateDonorShmem(sk->wp);

	/* event set will be updated inside SendMessageToNode */
	SendMessageToNode(sk);
}

/*
 * Try to send message to the particular node. Always updates event set. Will
 * send at least one message, if socket is ready.
 *
 * Can be used only for safekeepers in SS_ACTIVE state. State can be changed
 * in case of errors.
 */
static void
SendMessageToNode(Safekeeper *sk)
{
	Assert(sk->state == SS_ACTIVE);

	/*
	 * Note: we always send everything to the safekeeper until WOULDBLOCK or
	 * nothing left to send
	 */
	HandleActiveState(sk, WL_SOCKET_WRITEABLE);
}

/*
 * Broadcast new message to all caught-up safekeepers
 */
static void
BroadcastAppendRequest(WalProposer *wp)
{
	for (int i = 0; i < wp->n_safekeepers; i++)
		if (wp->safekeeper[i].state == SS_ACTIVE)
			SendMessageToNode(&wp->safekeeper[i]);
}

static void
PrepareAppendRequest(WalProposer *wp, AppendRequestHeader *req, XLogRecPtr beginLsn, XLogRecPtr endLsn)
{
	Assert(endLsn >= beginLsn);
	req->apm.tag = 'a';
	req->generation = wp->mconf.generation;
	req->term = wp->propTerm;
	req->beginLsn = beginLsn;
	req->endLsn = endLsn;
	req->commitLsn = wp->commitLsn;
	req->truncateLsn = wp->truncateLsn;
}

/*
 * Process all events happened in SS_ACTIVE state, update event set after that.
 */
static void
HandleActiveState(Safekeeper *sk, uint32 events)
{
	WalProposer *wp = sk->wp;

	/*
	 * Note: we don't known which socket awoke us (sk or nwr). However, as
	 * SendAppendRequests always tries to send at least one msg in
	 * SS_ACTIVE_SEND be careful not to go there if are only after sk
	 * response, otherwise it'd create busy loop of pings.
	 */
	if (events & WL_SOCKET_WRITEABLE || sk->active_state == SS_ACTIVE_READ_WAL)
		if (!SendAppendRequests(sk))
			return;

	if (events & WL_SOCKET_READABLE)
		if (!RecvAppendResponses(sk))
			return;

#if PG_VERSION_NUM >= 150000
	/* expected never to happen, c.f. walprop_pg_active_state_update_event_set */
	if (events & WL_SOCKET_CLOSED)
	{
		wp_log(WARNING, "connection to %s:%s in active state failed, got WL_SOCKET_CLOSED on neon_walreader socket",
			   sk->host, sk->port);
		ShutdownConnection(sk);
		return;
	}
#endif

	/* configures event set for yield whatever is the substate */
	wp->api.active_state_update_event_set(sk);
}

/*
 * Send WAL messages starting from sk->streamingAt until the end or non-writable
 * socket or neon_walreader blocks, whichever comes first; active_state is
 * updated accordingly. Caller should take care of updating event set. Even if
 * no unsent WAL is available, at least one empty message will be sent as a
 * heartbeat, if socket is ready.
 *
 * Resets state and kills the connections if any error on them is encountered.
 * Returns false in this case, true otherwise.
 */
static bool
SendAppendRequests(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;
	XLogRecPtr	endLsn;
	PGAsyncWriteResult writeResult;
	bool		sentAnything = false;
	AppendRequestHeader *req;

	if (sk->active_state == SS_ACTIVE_FLUSH)
	{
		if (!AsyncFlush(sk))

			/*
			 * AsyncFlush failed, that could happen if the socket is closed or
			 * we have nothing to write and should wait for writeable socket.
			 */
			return sk->state == SS_ACTIVE;

		/* Event set will be updated in the end of HandleActiveState */
		sk->active_state = SS_ACTIVE_SEND;
	}

	while (sk->streamingAt != wp->availableLsn || !sentAnything)
	{
		if (sk->active_state == SS_ACTIVE_SEND)
		{
			sentAnything = true;

			endLsn = sk->streamingAt;
			endLsn += MAX_SEND_SIZE;

			/* if we went beyond available WAL, back off */
			if (endLsn > wp->availableLsn)
			{
				endLsn = wp->availableLsn;
			}

			req = &sk->appendRequest;
			PrepareAppendRequest(sk->wp, &sk->appendRequest, sk->streamingAt, endLsn);

			wp_log(DEBUG5, "sending message len %ld beginLsn=%X/%X endLsn=%X/%X commitLsn=%X/%X truncateLsn=%X/%X to %s:%s",
				   req->endLsn - req->beginLsn,
				   LSN_FORMAT_ARGS(req->beginLsn),
				   LSN_FORMAT_ARGS(req->endLsn),
				   LSN_FORMAT_ARGS(req->commitLsn),
				   LSN_FORMAT_ARGS(wp->truncateLsn), sk->host, sk->port);

			resetStringInfo(&sk->outbuf);

			/* write AppendRequest header */
			PAMessageSerialize(wp, (ProposerAcceptorMessage *) req, &sk->outbuf, wp->config->proto_version);
			/* prepare for reading WAL into the outbuf */
			enlargeStringInfo(&sk->outbuf, req->endLsn - req->beginLsn);
			sk->active_state = SS_ACTIVE_READ_WAL;
		}

		if (sk->active_state == SS_ACTIVE_READ_WAL)
		{
			char	   *errmsg;
			int			req_len;

			req = &sk->appendRequest;
			req_len = req->endLsn - req->beginLsn;

			/*
			 * We send zero sized AppenRequests as heartbeats; don't wal_read
			 * for these.
			 */
			if (req_len > 0)
			{
				switch (wp->api.wal_read(sk,
										 &sk->outbuf.data[sk->outbuf.len],
										 req->beginLsn,
										 req_len,
										 &errmsg))
				{
					case NEON_WALREAD_SUCCESS:
						break;
					case NEON_WALREAD_WOULDBLOCK:
						return true;
					case NEON_WALREAD_ERROR:
						wp_log(WARNING, "WAL reading for node %s:%s failed: %s",
							   sk->host, sk->port, errmsg);
						ShutdownConnection(sk);
						return false;
					default:
						Assert(false);
				}
			}

			sk->outbuf.len += req_len;

			writeResult = wp->api.conn_async_write(sk, sk->outbuf.data, sk->outbuf.len);

			/* Mark current message as sent, whatever the result is */
			sk->streamingAt = req->endLsn;

			switch (writeResult)
			{
				case PG_ASYNC_WRITE_SUCCESS:
					/* Continue writing the next message */
					sk->active_state = SS_ACTIVE_SEND;
					break;

				case PG_ASYNC_WRITE_TRY_FLUSH:

					/*
					 * We still need to call PQflush some more to finish the
					 * job. Caller function will handle this by setting right
					 * event set.
					 */
					sk->active_state = SS_ACTIVE_FLUSH;
					return true;

				case PG_ASYNC_WRITE_FAIL:
					wp_log(WARNING, "failed to send to node %s:%s in %s state: %s",
						   sk->host, sk->port, FormatSafekeeperState(sk),
						   wp->api.conn_error_message(sk));
					ShutdownConnection(sk);
					return false;
				default:
					Assert(false);
					return false;
			}
		}
	}

	return true;
}

/*
 * Receive and process all available feedback.
 *
 * Resets state and kills the connection if any error on it is encountered.
 * Returns false in this case, true otherwise.
 *
 * NB: This function can call SendMessageToNode and produce new messages.
 */
static bool
RecvAppendResponses(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;
	bool		readAnything = false;

	while (true)
	{
		/*
		 * If our reading doesn't immediately succeed, any necessary error
		 * handling or state setting is taken care of. We can leave any other
		 * work until later.
		 */
		sk->appendResponse.apm.tag = 'a';
		if (!AsyncReadMessage(sk, (AcceptorProposerMessage *) &sk->appendResponse))
			break;

		wp_log(DEBUG2, "received message term=" INT64_FORMAT " flushLsn=%X/%X commitLsn=%X/%X from %s:%s",
			   sk->appendResponse.term,
			   LSN_FORMAT_ARGS(sk->appendResponse.flushLsn),
			   LSN_FORMAT_ARGS(sk->appendResponse.commitLsn),
			   sk->host, sk->port);

		readAnything = true;

		/* should never happen: sk is expected to send ERROR instead */
		if (sk->appendResponse.generation != wp->mconf.generation)
		{
			wp_log(FATAL, "safekeeper {id = %lu, ep = %s:%s} sent response with generation %u, expected %u",
				   sk->greetResponse.nodeId, sk->host, sk->port,
				   sk->appendResponse.generation, wp->mconf.generation);
		}

		if (sk->appendResponse.term > wp->propTerm)
		{
			/*
			 *
			 * Term has changed to higher one, probably another compute is
			 * running. If this is the case we could PANIC as well because
			 * likely it inserted some data and our basebackup is unsuitable
			 * anymore. However, we also bump term manually (term_bump
			 * endpoint) on safekeepers for migration purposes, in this case
			 * we do want compute to stay alive. So restart walproposer with
			 * FATAL instead of panicking; if basebackup is spoiled next
			 * election will notice this.
			 */
			wp_log(FATAL, "WAL acceptor %s:%s with term " INT64_FORMAT " rejected our request, our term " INT64_FORMAT ", meaning another compute is running at the same time, and it conflicts with us",
				   sk->host, sk->port,
				   sk->appendResponse.term, wp->propTerm);
		}

		HandleSafekeeperResponse(wp, sk);
	}

	if (!readAnything)
		return sk->state == SS_ACTIVE;

	return sk->state == SS_ACTIVE;
}

#define psfeedback_log(fmt, key, ...) \
	wp_log(DEBUG2, "ParsePageserverFeedbackMessage: %s " fmt, key, __VA_ARGS__)

/* Parse a PageserverFeedback message, or the PageserverFeedback part of an AppendResponse */
static void
ParsePageserverFeedbackMessage(WalProposer *wp, StringInfo reply_message, PageserverFeedback *ps_feedback)
{
	uint8		nkeys;
	int			i;

	/* initialize the struct before parsing */
	memset(ps_feedback, 0, sizeof(PageserverFeedback));
	ps_feedback->present = true;

	/* get number of custom keys */
	nkeys = pq_getmsgbyte(reply_message);

	for (i = 0; i < nkeys; i++)
	{
		const char *key = pq_getmsgrawstring(reply_message);
		unsigned int value_len = pq_getmsgint(reply_message, sizeof(int32));

		if (strcmp(key, "current_timeline_size") == 0)
		{
			Assert(value_len == sizeof(int64));
			ps_feedback->currentClusterSize = pq_getmsgint64(reply_message);
			psfeedback_log(UINT64_FORMAT, key, ps_feedback->currentClusterSize);
		}
		else if ((strcmp(key, "ps_writelsn") == 0) || (strcmp(key, "last_received_lsn") == 0))
		{
			Assert(value_len == sizeof(int64));
			ps_feedback->last_received_lsn = pq_getmsgint64(reply_message);
			psfeedback_log("%X/%X", key, LSN_FORMAT_ARGS(ps_feedback->last_received_lsn));
		}
		else if ((strcmp(key, "ps_flushlsn") == 0) || (strcmp(key, "disk_consistent_lsn") == 0))
		{
			Assert(value_len == sizeof(int64));
			ps_feedback->disk_consistent_lsn = pq_getmsgint64(reply_message);
			psfeedback_log("%X/%X", key, LSN_FORMAT_ARGS(ps_feedback->disk_consistent_lsn));
		}
		else if ((strcmp(key, "ps_applylsn") == 0) || (strcmp(key, "remote_consistent_lsn") == 0))
		{
			Assert(value_len == sizeof(int64));
			ps_feedback->remote_consistent_lsn = pq_getmsgint64(reply_message);
			psfeedback_log("%X/%X", key, LSN_FORMAT_ARGS(ps_feedback->remote_consistent_lsn));
		}
		else if ((strcmp(key, "ps_replytime") == 0) || (strcmp(key, "replytime") == 0))
		{
			Assert(value_len == sizeof(int64));
			ps_feedback->replytime = pq_getmsgint64(reply_message);
			psfeedback_log("%s", key, timestamptz_to_str(ps_feedback->replytime));
		}
		else if (strcmp(key, "shard_number") == 0)
		{
			Assert(value_len == sizeof(uint32));
			ps_feedback->shard_number = pq_getmsgint(reply_message, sizeof(uint32));
			psfeedback_log("%u", key, ps_feedback->shard_number);
		}
		else if (strcmp(key, "corruption_detected") == 0)
		{
			Assert(value_len == 1);
			ps_feedback->corruption_detected = pq_getmsgbyte(reply_message) != 0;
			psfeedback_log("%s", key, ps_feedback->corruption_detected ? "true" : "false");
		}
		else
		{
			/*
			 * Skip unknown keys to support backward compatibile protocol
			 * changes
			 */
			wp_log(LOG, "ParsePageserverFeedbackMessage: unknown key: %s len %d", key, value_len);
			pq_getmsgbytes(reply_message, value_len);
		};
	}
}

/*
 * Get minimum of flushed LSNs of all safekeepers, which is the LSN of the
 * last WAL record that can be safely discarded.
 */
static XLogRecPtr
CalculateMinFlushLsn(WalProposer *wp)
{
	XLogRecPtr	lsn = wp->n_safekeepers > 0
		? wp->safekeeper[0].appendResponse.flushLsn
		: InvalidXLogRecPtr;

	for (int i = 1; i < wp->n_safekeepers; i++)
	{
		lsn = Min(lsn, wp->safekeeper[i].appendResponse.flushLsn);
	}
	return lsn;
}

/*
 * GetAcknowledgedByQuorumWALPosition for a single member set `mset`.
 *
 * `msk` is the member -> safekeeper mapping for mset, i.e. members_safekeepers
 * or new_members_safekeepers.
 */
static XLogRecPtr
GetCommittedMset(WalProposer *wp, MemberSet *mset, Safekeeper **msk)
{
	XLogRecPtr	responses[MAX_SAFEKEEPERS];

	/*
	 * Ascending sort acknowledged LSNs.
	 */
	Assert(mset->len <= MAX_SAFEKEEPERS);
	for (uint32 i = 0; i < mset->len; i++)
	{
		Safekeeper *sk = msk[i];

		/*
		 * Like in Raft, we aren't allowed to commit entries from previous
		 * terms, so ignore reported LSN until it gets to propTermStartLsn.
		 *
		 * Note: we ignore sk state, which is ok: before first ack flushLsn is
		 * 0, and later we just preserve value across reconnections. It would
		 * be ok to check for SS_ACTIVE as well.
		 */
		if (sk != NULL && sk->appendResponse.flushLsn >= wp->propTermStartLsn)
		{
			responses[i] = sk->appendResponse.flushLsn;
		}
		else
		{
			responses[i] = 0;
		}
	}
	qsort(responses, mset->len, sizeof(XLogRecPtr), CompareLsn);

	/*
	 * And get value committed by the quorum. A way to view this: to get the
	 * highest value committed on the quorum, in the ordered array we skip n -
	 * n_quorum elements to get to the first (lowest) value present on all sks
	 * of the highest quorum.
	 */
	return responses[mset->len - MsetQuorum(mset)];
}

/*
 * Calculate WAL position acknowledged by quorum, i.e. which may be regarded
 * committed.
 *
 * Zero may be returned when there is no quorum of nodes recovered to term start
 * lsn which sent feedback yet.
 */
static XLogRecPtr
GetAcknowledgedByQuorumWALPosition(WalProposer *wp)
{
	XLogRecPtr	committed;

	/* legacy: generations disabled */
	if (!WalProposerGenerationsEnabled(wp) && wp->mconf.generation == INVALID_GENERATION)
	{
		XLogRecPtr	responses[MAX_SAFEKEEPERS];

		/*
		 * Sort acknowledged LSNs
		 */
		for (int i = 0; i < wp->n_safekeepers; i++)
		{
			/*
			 * Like in Raft, we aren't allowed to commit entries from previous
			 * terms, so ignore reported LSN until it gets to
			 * propTermStartLsn.
			 *
			 * Note: we ignore sk state, which is ok: before first ack
			 * flushLsn is 0, and later we just preserve value across
			 * reconnections. It would be ok to check for SS_ACTIVE as well.
			 */
			responses[i] = wp->safekeeper[i].appendResponse.flushLsn >= wp->propTermStartLsn ? wp->safekeeper[i].appendResponse.flushLsn : 0;
		}
		qsort(responses, wp->n_safekeepers, sizeof(XLogRecPtr), CompareLsn);

		/*
		 * Get the smallest LSN committed by quorum
		 */
		return responses[wp->n_safekeepers - wp->quorum];
	}

	committed = GetCommittedMset(wp, &wp->mconf.members, wp->members_safekeepers);
	if (wp->mconf.new_members.len > 0)
	{
		XLogRecPtr	new_mset_committed = GetCommittedMset(wp, &wp->mconf.new_members, wp->new_members_safekeepers);

		committed = Min(committed, new_mset_committed);
	}
	return committed;
}

/*
 * Return safekeeper with active connection from which WAL can be downloaded, or
 * none if it doesn't exist. donor_lsn is set to end position of the donor to
 * the best of our knowledge.
 */
static void
UpdateDonorShmem(WalProposer *wp)
{
	Safekeeper *donor = NULL;
	int			i;
	XLogRecPtr	donor_lsn = InvalidXLogRecPtr;

	if (wp->state < WPS_ELECTED)
	{
		wp_log(WARNING, "UpdateDonorShmem called before elections are won");
		return;
	}

	/*
	 * First, consider node which had determined our term start LSN as we know
	 * about its position immediately after election before any feedbacks are
	 * sent.
	 */
	if (wp->donor->state >= SS_WAIT_ELECTED)
	{
		donor = wp->donor;
		donor_lsn = wp->propTermStartLsn;
	}

	/*
	 * But also check feedbacks from all nodes with live connections and take
	 * the highest one. Note: if node sends feedbacks it already processed
	 * elected message so its term is fine.
	 */
	for (i = 0; i < wp->n_safekeepers; i++)
	{
		Safekeeper *sk = &wp->safekeeper[i];

		if (sk->state == SS_ACTIVE && sk->appendResponse.flushLsn > donor_lsn)
		{
			donor = sk;
			donor_lsn = sk->appendResponse.flushLsn;
		}
	}

	if (donor == NULL)
	{
		wp_log(WARNING, "UpdateDonorShmem didn't find a suitable donor, skipping");
		return;
	}
	wp->api.update_donor(wp, donor, donor_lsn);
}

/*
 * Process AppendResponse message from safekeeper.
 */
static void
HandleSafekeeperResponse(WalProposer *wp, Safekeeper *fromsk)
{
	XLogRecPtr	candidateTruncateLsn;
	XLogRecPtr	newCommitLsn;

	newCommitLsn = GetAcknowledgedByQuorumWALPosition(wp);
	if (newCommitLsn > wp->commitLsn)
	{
		wp->commitLsn = newCommitLsn;
		/* Send new value to all safekeepers. */
		BroadcastAppendRequest(wp);
	}

	/*
	 * Unlock syncrep waiters, update ps_feedback, CheckGracefulShutdown().
	 * The last one will terminate the process if the shutdown is requested
	 * and WAL is committed by the quorum. BroadcastAppendRequest() should be
	 * called to notify safekeepers about the new commitLsn.
	 */
	wp->api.process_safekeeper_feedback(wp, fromsk);

	/*
	 * Try to advance truncateLsn -- the last record flushed to all
	 * safekeepers.
	 *
	 * Advanced truncateLsn should be not higher than commitLsn. This prevents
	 * surprising violation of truncateLsn <= commitLsn invariant which might
	 * occur because commitLsn generally can't be advanced based on feedback
	 * from safekeeper who is still in the previous epoch (similar to 'leader
	 * can't commit entries from previous term' in Raft); 2)
	 */
	candidateTruncateLsn = CalculateMinFlushLsn(wp);
	candidateTruncateLsn = Min(candidateTruncateLsn, wp->commitLsn);
	if (candidateTruncateLsn > wp->truncateLsn)
	{
		wp->truncateLsn = candidateTruncateLsn;
	}

	/*
	 * Generally sync is done when majority reached propTermStartLsn so we
	 * committed it and made the majority aware of it, ensuring they are ready
	 * to give all WAL to pageserver. It would mean whichever majority is
	 * alive, there will be at least one safekeeper who is able to stream WAL
	 * to pageserver to make basebackup possible. However, since at the moment
	 * we don't have any good mechanism of defining the healthy and most
	 * advanced safekeeper who should push the wal into pageserver and
	 * basically the random one gets connected, to prevent hanging basebackup
	 * (due to pageserver connecting to not-synced-safekeeper) we currently
	 * wait for all seemingly alive safekeepers to get synced.
	 */
	if (wp->config->syncSafekeepers)
	{
		for (int i = 0; i < wp->n_safekeepers; i++)
		{
			Safekeeper *sk = &wp->safekeeper[i];
			bool		synced = sk->appendResponse.commitLsn >= wp->propTermStartLsn;

			/* alive safekeeper which is not synced yet; wait for it */
			if (sk->state != SS_OFFLINE && !synced)
				return;
		}

		if (newCommitLsn >= wp->propTermStartLsn)
		{
			/* A quorum of safekeepers has been synced! */

			/*
			 * Send empty message to broadcast latest truncateLsn to all
			 * safekeepers. This helps to finish next sync-safekeepers
			 * eailier, by skipping recovery step.
			 *
			 * We don't need to wait for response because it doesn't affect
			 * correctness, and TCP should be able to deliver the message to
			 * safekeepers in case of network working properly.
			 */
			BroadcastAppendRequest(wp);

			wp->api.finish_sync_safekeepers(wp, wp->propTermStartLsn);
			/* unreachable */
		}
	}
}

/* Serialize MembershipConfiguration into buf. */
static void
MembershipConfigurationSerialize(MembershipConfiguration *mconf, StringInfo buf)
{
	uint32		i;

	pq_sendint32(buf, mconf->generation);

	pq_sendint32(buf, mconf->members.len);
	for (i = 0; i < mconf->members.len; i++)
	{
		pq_sendint64(buf, mconf->members.m[i].node_id);
		pq_send_ascii_string(buf, mconf->members.m[i].host);
		pq_sendint16(buf, mconf->members.m[i].port);
	}

	/*
	 * There is no special mark for absent new_members; zero members in
	 * invalid, so zero len means absent.
	 */
	pq_sendint32(buf, mconf->new_members.len);
	for (i = 0; i < mconf->new_members.len; i++)
	{
		pq_sendint64(buf, mconf->new_members.m[i].node_id);
		pq_send_ascii_string(buf, mconf->new_members.m[i].host);
		pq_sendint16(buf, mconf->new_members.m[i].port);
	}
}

/* Serialize proposer -> acceptor message into buf using specified version */
static void
PAMessageSerialize(WalProposer *wp, ProposerAcceptorMessage *msg, StringInfo buf, int proto_version)
{
	/* both version are supported currently until we fully migrate to 3 */
	Assert(proto_version == 3 || proto_version == 2);

	resetStringInfo(buf);

	if (proto_version == 3)
	{
		/*
		 * v2 sends structs for some messages as is, so commonly send tag only
		 * for v3
		 */
		pq_sendint8(buf, msg->tag);

		switch (msg->tag)
		{
			case 'g':
				{
					ProposerGreeting *m = (ProposerGreeting *) msg;

					pq_send_ascii_string(buf, m->tenant_id);
					pq_send_ascii_string(buf, m->timeline_id);
					MembershipConfigurationSerialize(&m->mconf, buf);
					pq_sendint32(buf, m->pg_version);
					pq_sendint64(buf, m->system_id);
					pq_sendint32(buf, m->wal_seg_size);
					break;
				}
			case 'v':
				{
					VoteRequest *m = (VoteRequest *) msg;

					pq_sendint32(buf, m->generation);
					pq_sendint64(buf, m->term);
					break;

				}
			case 'e':
				{
					ProposerElected *m = (ProposerElected *) msg;

					pq_sendint32(buf, m->generation);
					pq_sendint64(buf, m->term);
					pq_sendint64(buf, m->startStreamingAt);
					pq_sendint32(buf, m->termHistory->n_entries);
					for (uint32 i = 0; i < m->termHistory->n_entries; i++)
					{
						pq_sendint64(buf, m->termHistory->entries[i].term);
						pq_sendint64(buf, m->termHistory->entries[i].lsn);
					}
					break;
				}
			case 'a':
				{
					/*
					 * Note: this serializes only AppendRequestHeader, caller
					 * is expected to append WAL data later.
					 */
					AppendRequestHeader *m = (AppendRequestHeader *) msg;

					pq_sendint32(buf, m->generation);
					pq_sendint64(buf, m->term);
					pq_sendint64(buf, m->beginLsn);
					pq_sendint64(buf, m->endLsn);
					pq_sendint64(buf, m->commitLsn);
					pq_sendint64(buf, m->truncateLsn);
					break;
				}
			default:
				wp_log(FATAL, "unexpected message type %c to serialize", msg->tag);
		}
		return;
	}

	if (proto_version == 2)
	{
		switch (msg->tag)
		{
			case 'g':
				{
					/* v2 sent struct as is */
					ProposerGreeting *m = (ProposerGreeting *) msg;
					ProposerGreetingV2 greetRequestV2;

					/* Fill also v2 struct. */
					greetRequestV2.tag = 'g';
					greetRequestV2.protocolVersion = proto_version;
					greetRequestV2.pgVersion = m->pg_version;

					/*
					 * v3 removed this field because it's easier to pass as
					 * libq or START_WAL_PUSH options
					 */
					memset(&greetRequestV2.proposerId, 0, sizeof(greetRequestV2.proposerId));
					greetRequestV2.systemId = wp->config->systemId;
					if (*m->timeline_id != '\0' &&
						!HexDecodeString(greetRequestV2.timeline_id, m->timeline_id, 16))
						wp_log(FATAL, "could not parse neon.timeline_id, %s", m->timeline_id);
					if (*m->tenant_id != '\0' &&
						!HexDecodeString(greetRequestV2.tenant_id, m->tenant_id, 16))
						wp_log(FATAL, "could not parse neon.tenant_id, %s", m->tenant_id);

					greetRequestV2.timeline = wp->config->pgTimeline;
					greetRequestV2.walSegSize = wp->config->wal_segment_size;

					pq_sendbytes(buf, (char *) &greetRequestV2, sizeof(greetRequestV2));
					break;
				}
			case 'v':
				{
					/* v2 sent struct as is */
					VoteRequest *m = (VoteRequest *) msg;
					VoteRequestV2 voteRequestV2;

					voteRequestV2.tag = m->pam.tag;
					voteRequestV2.term = m->term;
					/* removed field */
					memset(&voteRequestV2.proposerId, 0, sizeof(voteRequestV2.proposerId));
					pq_sendbytes(buf, (char *) &voteRequestV2, sizeof(voteRequestV2));
					break;
				}
			case 'e':
				{
					ProposerElected *m = (ProposerElected *) msg;

					pq_sendint64_le(buf, m->apm.tag);
					pq_sendint64_le(buf, m->term);
					pq_sendint64_le(buf, m->startStreamingAt);
					pq_sendint32_le(buf, m->termHistory->n_entries);
					for (int i = 0; i < m->termHistory->n_entries; i++)
					{
						pq_sendint64_le(buf, m->termHistory->entries[i].term);
						pq_sendint64_le(buf, m->termHistory->entries[i].lsn);
					}

					/*
					 * Removed timeline_start_lsn. Still send it as a valid
					 * value until safekeepers taking it from term history are
					 * deployed.
					 */
					pq_sendint64_le(buf, m->termHistory->entries[0].lsn);
					break;
				}
			case 'a':

				/*
				 * Note: this serializes only AppendRequestHeader, caller is
				 * expected to append WAL data later.
				 */
				{
					/* v2 sent struct as is */
					AppendRequestHeader *m = (AppendRequestHeader *) msg;
					AppendRequestHeaderV2 appendRequestHeaderV2;

					appendRequestHeaderV2.tag = m->apm.tag;
					appendRequestHeaderV2.term = m->term;
					appendRequestHeaderV2.epochStartLsn = 0;	/* removed field */
					appendRequestHeaderV2.beginLsn = m->beginLsn;
					appendRequestHeaderV2.endLsn = m->endLsn;
					appendRequestHeaderV2.commitLsn = m->commitLsn;
					appendRequestHeaderV2.truncateLsn = m->truncateLsn;
					/* removed field */
					memset(&appendRequestHeaderV2.proposerId, 0, sizeof(appendRequestHeaderV2.proposerId));

					pq_sendbytes(buf, (char *) &appendRequestHeaderV2, sizeof(appendRequestHeaderV2));
					break;
				}

			default:
				wp_log(FATAL, "unexpected message type %c to serialize", msg->tag);
		}
		return;
	}
	wp_log(FATAL, "unexpected proto_version %d", proto_version);
}

/*
 * Try to read CopyData message from i'th safekeeper, resetting connection on
 * failure.
 */
static bool
AsyncRead(Safekeeper *sk, char **buf, int *buf_size)
{
	WalProposer *wp = sk->wp;

	switch (wp->api.conn_async_read(sk, buf, buf_size))
	{
		case PG_ASYNC_READ_SUCCESS:
			return true;

		case PG_ASYNC_READ_TRY_AGAIN:
			/* WL_SOCKET_READABLE is always set during copyboth */
			return false;

		case PG_ASYNC_READ_FAIL:
			wp_log(WARNING, "failed to read from node %s:%s in %s state: %s", sk->host,
				   sk->port, FormatSafekeeperState(sk),
				   wp->api.conn_error_message(sk));
			ShutdownConnection(sk);
			return false;
	}
	Assert(false);
	return false;
}

/* Deserialize membership configuration from buf to mconf. */
static void
MembershipConfigurationDeserialize(MembershipConfiguration *mconf, StringInfo buf)
{
	uint32		i;

	mconf->generation = pq_getmsgint32(buf);
	mconf->members.len = pq_getmsgint32(buf);
	mconf->members.m = palloc0(sizeof(SafekeeperId) * mconf->members.len);
	for (i = 0; i < mconf->members.len; i++)
	{
		const char *buf_host;

		mconf->members.m[i].node_id = pq_getmsgint64(buf);
		buf_host = pq_getmsgrawstring(buf);
		strlcpy(mconf->members.m[i].host, buf_host, sizeof(mconf->members.m[i].host));
		mconf->members.m[i].port = pq_getmsgint16(buf);
	}
	mconf->new_members.len = pq_getmsgint32(buf);
	mconf->new_members.m = palloc0(sizeof(SafekeeperId) * mconf->new_members.len);
	for (i = 0; i < mconf->new_members.len; i++)
	{
		const char *buf_host;

		mconf->new_members.m[i].node_id = pq_getmsgint64(buf);
		buf_host = pq_getmsgrawstring(buf);
		strlcpy(mconf->new_members.m[i].host, buf_host, sizeof(mconf->new_members.m[i].host));
		mconf->new_members.m[i].port = pq_getmsgint16(buf);
	}
}

/*
 * Read next message with known type into provided struct, by reading a CopyData
 * block from the safekeeper's postgres connection, returning whether the read
 * was successful.
 *
 * If the read needs more polling, we return 'false' and keep the state
 * unmodified, waiting until it becomes read-ready to try again. If it fully
 * failed, a warning is emitted and the connection is reset.
 *
 * Note: it pallocs if needed, i.e. for AcceptorGreeting and VoteResponse fields.
 */
static bool
AsyncReadMessage(Safekeeper *sk, AcceptorProposerMessage *anymsg)
{
	WalProposer *wp = sk->wp;

	char	   *buf;
	int			buf_size;
	uint8		tag;
	StringInfoData s;

	if (!(AsyncRead(sk, &buf, &buf_size)))
		return false;
	sk->latestMsgReceivedAt = wp->api.get_current_timestamp(wp);

	/* parse it */
	s.data = buf;
	s.len = buf_size;
	s.maxlen = buf_size;
	s.cursor = 0;

	if (wp->config->proto_version == 3)
	{
		tag = pq_getmsgbyte(&s);
		if (tag != anymsg->tag)
		{
			wp_log(WARNING, "unexpected message tag %c from node %s:%s in state %s", (char) tag, sk->host,
				   sk->port, FormatSafekeeperState(sk));
			ResetConnection(sk);
			return false;
		}
		switch (tag)
		{
			case 'g':
				{
					AcceptorGreeting *msg = (AcceptorGreeting *) anymsg;

					msg->nodeId = pq_getmsgint64(&s);
					MembershipConfigurationDeserialize(&msg->mconf, &s);
					msg->term = pq_getmsgint64(&s);
					pq_getmsgend(&s);
					return true;
				}
			case 'v':
				{
					VoteResponse *msg = (VoteResponse *) anymsg;

					msg->generation = pq_getmsgint32(&s);
					msg->term = pq_getmsgint64(&s);
					msg->voteGiven = pq_getmsgbyte(&s);
					msg->flushLsn = pq_getmsgint64(&s);
					msg->truncateLsn = pq_getmsgint64(&s);
					msg->termHistory.n_entries = pq_getmsgint32(&s);
					msg->termHistory.entries = palloc(sizeof(TermSwitchEntry) * msg->termHistory.n_entries);
					for (uint32 i = 0; i < msg->termHistory.n_entries; i++)
					{
						msg->termHistory.entries[i].term = pq_getmsgint64(&s);
						msg->termHistory.entries[i].lsn = pq_getmsgint64(&s);
					}
					pq_getmsgend(&s);
					return true;
				}
			case 'a':
				{
					AppendResponse *msg = (AppendResponse *) anymsg;

					msg->generation = pq_getmsgint32(&s);
					msg->term = pq_getmsgint64(&s);
					msg->flushLsn = pq_getmsgint64(&s);
					msg->commitLsn = pq_getmsgint64(&s);
					msg->hs.ts = pq_getmsgint64(&s);
					msg->hs.xmin.value = pq_getmsgint64(&s);
					msg->hs.catalog_xmin.value = pq_getmsgint64(&s);
					if (s.len > s.cursor)
						ParsePageserverFeedbackMessage(wp, &s, &msg->ps_feedback);
					else
						msg->ps_feedback.present = false;
					pq_getmsgend(&s);
					return true;
				}
			default:
				{
					wp_log(FATAL, "unexpected message tag %c to read", (char) tag);
					return false;
				}
		}
	}
	else if (wp->config->proto_version == 2)
	{
		tag = pq_getmsgint64_le(&s);
		if (tag != anymsg->tag)
		{
			wp_log(WARNING, "unexpected message tag %c from node %s:%s in state %s", (char) tag, sk->host,
				   sk->port, FormatSafekeeperState(sk));
			ResetConnection(sk);
			return false;
		}
		switch (tag)
		{
			case 'g':
				{
					AcceptorGreeting *msg = (AcceptorGreeting *) anymsg;

					msg->term = pq_getmsgint64_le(&s);
					msg->nodeId = pq_getmsgint64_le(&s);
					pq_getmsgend(&s);
					return true;
				}

			case 'v':
				{
					VoteResponse *msg = (VoteResponse *) anymsg;

					msg->term = pq_getmsgint64_le(&s);
					msg->voteGiven = pq_getmsgint64_le(&s);
					msg->flushLsn = pq_getmsgint64_le(&s);
					msg->truncateLsn = pq_getmsgint64_le(&s);
					msg->termHistory.n_entries = pq_getmsgint32_le(&s);
					msg->termHistory.entries = palloc(sizeof(TermSwitchEntry) * msg->termHistory.n_entries);
					for (int i = 0; i < msg->termHistory.n_entries; i++)
					{
						msg->termHistory.entries[i].term = pq_getmsgint64_le(&s);
						msg->termHistory.entries[i].lsn = pq_getmsgint64_le(&s);
					}
					pq_getmsgint64_le(&s);	/* timelineStartLsn */
					pq_getmsgend(&s);
					return true;
				}

			case 'a':
				{
					AppendResponse *msg = (AppendResponse *) anymsg;

					msg->term = pq_getmsgint64_le(&s);
					msg->flushLsn = pq_getmsgint64_le(&s);
					msg->commitLsn = pq_getmsgint64_le(&s);
					msg->hs.ts = pq_getmsgint64_le(&s);
					msg->hs.xmin.value = pq_getmsgint64_le(&s);
					msg->hs.catalog_xmin.value = pq_getmsgint64_le(&s);
					if (s.len > s.cursor)
						ParsePageserverFeedbackMessage(wp, &s, &msg->ps_feedback);
					else
						msg->ps_feedback.present = false;
					pq_getmsgend(&s);
					return true;
				}

			default:
				{
					wp_log(FATAL, "unexpected message tag %c to read", (char) tag);
					return false;
				}
		}
	}
	wp_log(FATAL, "unsupported proto_version %d", wp->config->proto_version);
	return false;				/* keep the compiler quiet */
}

/*
 * Blocking equivalent to AsyncWrite.
 *
 * We use this everywhere messages are small enough that they should fit in a
 * single packet.
 */
static bool
BlockingWrite(Safekeeper *sk, void *msg, size_t msg_size, SafekeeperState success_state)
{
	WalProposer *wp = sk->wp;
	uint32		sk_events;
	uint32		nwr_events;

	if (!wp->api.conn_blocking_write(sk, msg, msg_size))
	{
		wp_log(WARNING, "failed to send to node %s:%s in %s state: %s",
			   sk->host, sk->port, FormatSafekeeperState(sk),
			   wp->api.conn_error_message(sk));
		ShutdownConnection(sk);
		return false;
	}

	sk->state = success_state;

	/*
	 * If the new state will be waiting for events to happen, update the event
	 * set to wait for those
	 */
	SafekeeperStateDesiredEvents(sk, &sk_events, &nwr_events);

	/*
	 * nwr_events is relevant only during SS_ACTIVE which doesn't use
	 * BlockingWrite
	 */
	Assert(!nwr_events);
	if (sk_events)
		wp->api.update_event_set(sk, sk_events);

	return true;
}

/*
 * Starts a write into the 'i'th safekeeper's postgres connection, moving to
 * flush_state (adjusting eventset) if write still needs flushing.
 *
 * Returns false if sending is unfinished (requires flushing or conn failed).
 * Upon failure, a warning is emitted and the connection is reset.
 */
static bool
AsyncWrite(Safekeeper *sk, void *msg, size_t msg_size, SafekeeperState flush_state)
{
	WalProposer *wp = sk->wp;

	switch (wp->api.conn_async_write(sk, msg, msg_size))
	{
		case PG_ASYNC_WRITE_SUCCESS:
			return true;
		case PG_ASYNC_WRITE_TRY_FLUSH:

			/*
			 * We still need to call PQflush some more to finish the job; go
			 * to the appropriate state. Update the event set at the bottom of
			 * this function
			 */
			sk->state = flush_state;
			wp->api.update_event_set(sk, WL_SOCKET_READABLE | WL_SOCKET_WRITEABLE);
			return false;
		case PG_ASYNC_WRITE_FAIL:
			wp_log(WARNING, "failed to send to node %s:%s in %s state: %s",
				   sk->host, sk->port, FormatSafekeeperState(sk),
				   wp->api.conn_error_message(sk));
			ShutdownConnection(sk);
			return false;
		default:
			Assert(false);
			return false;
	}
}

/*
 * Flushes a previous call to AsyncWrite. This only needs to be called when the
 * socket becomes read or write ready *after* calling AsyncWrite.
 *
 * If flushing successfully completes returns true, otherwise false. Event set
 * is updated only if connection fails, otherwise caller should manually unset
 * WL_SOCKET_WRITEABLE.
 */
static bool
AsyncFlush(Safekeeper *sk)
{
	WalProposer *wp = sk->wp;

	/*---
	 * PQflush returns:
	 *   0 if successful                    [we're good to move on]
	 *   1 if unable to send everything yet [call PQflush again]
	 *  -1 if it failed                     [emit an error]
	 */
	switch (wp->api.conn_flush(sk))
	{
		case 0:
			/* flush is done */
			return true;
		case 1:
			/* Nothing to do; try again when the socket's ready */
			return false;
		case -1:
			wp_log(WARNING, "failed to flush write to node %s:%s in %s state: %s",
				   sk->host, sk->port, FormatSafekeeperState(sk),
				   wp->api.conn_error_message(sk));
			ResetConnection(sk);
			return false;
		default:
			Assert(false);
			return false;
	}
}

static int
CompareLsn(const void *a, const void *b)
{
	XLogRecPtr	lsn1 = *((const XLogRecPtr *) a);
	XLogRecPtr	lsn2 = *((const XLogRecPtr *) b);

	if (lsn1 < lsn2)
		return -1;
	else if (lsn1 == lsn2)
		return 0;
	else
		return 1;
}

/* Returns a human-readable string corresonding to the SafekeeperState
 *
 * The string should not be freed.
 *
 * The strings are intended to be used as a prefix to "state", e.g.:
 *
 *   wp_log(LOG, "currently in %s state", FormatSafekeeperState(sk));
 *
 * If this sort of phrasing doesn't fit the message, instead use something like:
 *
 *   wp_log(LOG, "currently in state [%s]", FormatSafekeeperState(sk));
 */
static char *
FormatSafekeeperState(Safekeeper *sk)
{
	char	   *return_val = NULL;

	switch (sk->state)
	{
		case SS_OFFLINE:
			return_val = "offline";
			break;
		case SS_CONNECTING_READ:
		case SS_CONNECTING_WRITE:
			return_val = "connecting";
			break;
		case SS_WAIT_EXEC_RESULT:
			return_val = "receiving query result";
			break;
		case SS_HANDSHAKE_RECV:
			return_val = "handshake (receiving)";
			break;
		case SS_WAIT_VOTING:
			return_val = "voting";
			break;
		case SS_WAIT_VERDICT:
			return_val = "wait-for-verdict";
			break;
		case SS_SEND_ELECTED_FLUSH:
			return_val = "send-announcement-flush";
			break;
		case SS_WAIT_ELECTED:
			return_val = "idle";
			break;
		case SS_ACTIVE:
			switch (sk->active_state)
			{
				case SS_ACTIVE_SEND:
					return_val = "active send";
					break;
				case SS_ACTIVE_READ_WAL:
					return_val = "active read WAL";
					break;
				case SS_ACTIVE_FLUSH:
					return_val = "active flush";
					break;
			}
			break;
	}

	Assert(return_val != NULL);

	return return_val;
}

/* Asserts that the provided events are expected for given safekeeper's state */
static void
AssertEventsOkForState(uint32 events, Safekeeper *sk)
{
	uint32		sk_events;
	uint32		nwr_events;
	uint32		expected;
	bool		events_ok_for_state;	/* long name so the `Assert` is more
										 * clear later */
	WalProposer *wp = sk->wp;

	SafekeeperStateDesiredEvents(sk, &sk_events, &nwr_events);

	/*
	 * Without one more level of notify target indirection we have no way to
	 * distinguish which socket woke up us, so just union expected events.
	 */
	expected = sk_events | nwr_events;
	events_ok_for_state = ((events & expected) != 0);

	if (!events_ok_for_state)
	{
		/*
		 * To give a descriptive message in the case of failure, we use elog
		 * and then an assertion that's guaranteed to fail.
		 */
		wp_log(WARNING, "events %s mismatched for safekeeper %s:%s in state [%s]",
			   FormatEvents(wp, events), sk->host, sk->port, FormatSafekeeperState(sk));
		Assert(events_ok_for_state);
	}
}

/* Returns the set of events for both safekeeper (sk_events) and neon_walreader
 * (nwr_events) sockets a safekeeper in this state should be waiting on.
 *
 * This will return WL_NO_EVENTS (= 0) for some events. */
void
SafekeeperStateDesiredEvents(Safekeeper *sk, uint32 *sk_events, uint32 *nwr_events)
{
	WalProposer *wp = sk->wp;

	*nwr_events = 0;			/* nwr_events is empty for most states */

	/* If the state doesn't have a modifier, we can check the base state */
	switch (sk->state)
	{
			/* Connecting states say what they want in the name */
		case SS_CONNECTING_READ:
			*sk_events = WL_SOCKET_READABLE;
			return;
		case SS_CONNECTING_WRITE:
			*sk_events = WL_SOCKET_WRITEABLE;
			return;

			/* Reading states need the socket to be read-ready to continue */
		case SS_WAIT_EXEC_RESULT:
		case SS_HANDSHAKE_RECV:
		case SS_WAIT_VERDICT:
			*sk_events = WL_SOCKET_READABLE;
			return;

			/*
			 * Idle states use read-readiness as a sign that the connection
			 * has been disconnected.
			 */
		case SS_WAIT_VOTING:
		case SS_WAIT_ELECTED:
			*sk_events = WL_SOCKET_READABLE;
			return;

		case SS_SEND_ELECTED_FLUSH:
			*sk_events = WL_SOCKET_READABLE | WL_SOCKET_WRITEABLE;
			return;

		case SS_ACTIVE:
			switch (sk->active_state)
			{
					/*
					 * Everything is sent; we just wait for sk responses and
					 * latch.
					 *
					 * Note: this assumes we send all available WAL to
					 * safekeeper in one wakeup (unless it blocks). Otherwise
					 * we would want WL_SOCKET_WRITEABLE here to finish the
					 * work.
					 */
				case SS_ACTIVE_SEND:
					*sk_events = WL_SOCKET_READABLE;
					/* c.f. walprop_pg_active_state_update_event_set */
#if PG_VERSION_NUM >= 150000
					if (wp->api.wal_reader_events(sk))
						*nwr_events = WL_SOCKET_CLOSED;
#endif							/* on PG 14 nwr_events remains 0 */
					return;

					/*
					 * Waiting for neon_walreader socket, but we still read
					 * responses from sk socket.
					 */
				case SS_ACTIVE_READ_WAL:
					*sk_events = WL_SOCKET_READABLE;
					*nwr_events = wp->api.wal_reader_events(sk);
					return;

					/*
					 * Need to flush the sk socket, so ignore neon_walreader
					 * one and set write interest on sk.
					 */
				case SS_ACTIVE_FLUSH:
					*sk_events = WL_SOCKET_READABLE | WL_SOCKET_WRITEABLE;
#if PG_VERSION_NUM >= 150000
					/* c.f. walprop_pg_active_state_update_event_set */
					if (wp->api.wal_reader_events(sk))
						*nwr_events = WL_SOCKET_CLOSED;
#endif							/* on PG 14 nwr_events remains 0 */
					return;
			}
			return;

			/* The offline state expects no events. */
		case SS_OFFLINE:
			*sk_events = 0;
			return;

		default:
			Assert(false);
	}
}

/* Returns a human-readable string corresponding to the event set
 *
 * If the events do not correspond to something set as the `events` field of a `WaitEvent`, the
 * returned string may be meaingless.
 *
 * The string should not be freed. It should also not be expected to remain the same between
 * function calls. */
static char *
FormatEvents(WalProposer *wp, uint32 events)
{
	static char return_str[8];

	/* Helper variable to check if there's extra bits */
	uint32		all_flags = WL_LATCH_SET
		| WL_SOCKET_READABLE
		| WL_SOCKET_WRITEABLE
		| WL_TIMEOUT
		| WL_POSTMASTER_DEATH
		| WL_EXIT_ON_PM_DEATH
		| WL_SOCKET_CONNECTED;

	/*
	 * The formatting here isn't supposed to be *particularly* useful -- it's
	 * just to give an sense of what events have been triggered without
	 * needing to remember your powers of two.
	 */

	return_str[0] = (events & WL_LATCH_SET) ? 'L' : '_';
	return_str[1] = (events & WL_SOCKET_READABLE) ? 'R' : '_';
	return_str[2] = (events & WL_SOCKET_WRITEABLE) ? 'W' : '_';
	return_str[3] = (events & WL_TIMEOUT) ? 'T' : '_';
	return_str[4] = (events & WL_POSTMASTER_DEATH) ? 'D' : '_';
	return_str[5] = (events & WL_EXIT_ON_PM_DEATH) ? 'E' : '_';
	return_str[5] = (events & WL_SOCKET_CONNECTED) ? 'C' : '_';

	if (events & (~all_flags))
	{
		wp_log(WARNING, "event formatting found unexpected component %d",
			   events & (~all_flags));
		return_str[6] = '*';
		return_str[7] = '\0';
	}
	else
		return_str[6] = '\0';

	return (char *) &return_str;
}

/* Dump mconf as toml for observability / debugging. Result is palloc'ed. */
static char *
MembershipConfigurationToString(MembershipConfiguration *mconf)
{
	StringInfoData s;
	uint32		i;

	initStringInfo(&s);
	appendStringInfo(&s, "{gen = %u", mconf->generation);
	appendStringInfoString(&s, ", members = [");
	for (i = 0; i < mconf->members.len; i++)
	{
		if (i > 0)
			appendStringInfoString(&s, ", ");
		appendStringInfo(&s, "{node_id = %lu", mconf->members.m[i].node_id);
		appendStringInfo(&s, ", host = %s", mconf->members.m[i].host);
		appendStringInfo(&s, ", port = %u }", mconf->members.m[i].port);
	}
	appendStringInfo(&s, "], new_members = [");
	for (i = 0; i < mconf->new_members.len; i++)
	{
		if (i > 0)
			appendStringInfoString(&s, ", ");
		appendStringInfo(&s, "{node_id = %lu", mconf->new_members.m[i].node_id);
		appendStringInfo(&s, ", host = %s", mconf->new_members.m[i].host);
		appendStringInfo(&s, ", port = %u }", mconf->new_members.m[i].port);
	}
	appendStringInfoString(&s, "]}");
	return s.data;
}

static void
MembershipConfigurationCopy(MembershipConfiguration *src, MembershipConfiguration *dst)
{
	dst->generation = src->generation;
	dst->members.len = src->members.len;
	dst->members.m = palloc0(sizeof(SafekeeperId) * dst->members.len);
	memcpy(dst->members.m, src->members.m, sizeof(SafekeeperId) * dst->members.len);
	dst->new_members.len = src->new_members.len;
	dst->new_members.m = palloc0(sizeof(SafekeeperId) * dst->new_members.len);
	memcpy(dst->new_members.m, src->new_members.m, sizeof(SafekeeperId) * dst->new_members.len);
}

static void
MembershipConfigurationFree(MembershipConfiguration *mconf)
{
	if (mconf->members.m)
		pfree(mconf->members.m);
	mconf->members.m = NULL;
	if (mconf->new_members.m)
		pfree(mconf->new_members.m);
	mconf->new_members.m = NULL;
}
