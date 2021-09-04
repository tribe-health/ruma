use std::{collections::BTreeSet, convert::TryFrom, sync::Arc};

use js_int::{int, Int};
use ruma_events::{
    room::{
        create::CreateEventContent,
        join_rules::{JoinRule, JoinRulesEventContent},
        member::{MembershipState, ThirdPartyInvite},
        power_levels::PowerLevelsEventContent,
        third_party_invite::ThirdPartyInviteEventContent,
    },
    EventType,
};
use ruma_identifiers::{RoomVersionId, UserId};
use tracing::{debug, error, info, warn};

use crate::{room_version::RoomVersion, Error, Event, Result, StateMap};

/// For the given event `kind` what are the relevant auth events that are needed to authenticate
/// this `content`.
pub fn auth_types_for_event(
    kind: &EventType,
    sender: &UserId,
    state_key: Option<String>,
    content: serde_json::Value,
) -> Vec<(EventType, String)> {
    if kind == &EventType::RoomCreate {
        return vec![];
    }

    let mut auth_types = vec![
        (EventType::RoomPowerLevels, "".to_owned()),
        (EventType::RoomMember, sender.to_string()),
        (EventType::RoomCreate, "".to_owned()),
    ];

    if kind == &EventType::RoomMember {
        if let Some(state_key) = state_key {
            if let Some(Ok(membership)) = content
                .get("membership")
                .map(|m| serde_json::from_value::<MembershipState>(m.clone()))
            {
                if [MembershipState::Join, MembershipState::Invite].contains(&membership) {
                    let key = (EventType::RoomJoinRules, "".to_owned());
                    if !auth_types.contains(&key) {
                        auth_types.push(key)
                    }
                }

                let key = (EventType::RoomMember, state_key);
                if !auth_types.contains(&key) {
                    auth_types.push(key)
                }

                if membership == MembershipState::Invite {
                    if let Some(Ok(t_id)) = content
                        .get("third_party_invite")
                        .map(|t| serde_json::from_value::<ThirdPartyInvite>(t.clone()))
                    {
                        let key = (EventType::RoomThirdPartyInvite, t_id.signed.token);
                        if !auth_types.contains(&key) {
                            auth_types.push(key)
                        }
                    }
                }
            }
        }
    }

    auth_types
}

/// Authenticate the incoming `event`.
///
/// The steps of authentication are:
///
/// * check that the event is being authenticated for the correct room
/// * check that the events signatures are valid
/// * then there are checks for specific event types
///
/// The `fetch_state` closure should gather state from a state snapshot. We need to know if the
/// event passes auth against some state not a recursive collection of auth_events fields.
///
/// ## Returns
///
/// This returns an `Error` only when serialization fails or some other fatal outcome.
pub fn auth_check<E, F>(
    room_version: &RoomVersion,
    incoming_event: &Arc<E>,
    prev_event: Option<Arc<E>>,
    current_third_party_invite: Option<Arc<E>>,
    fetch_state: F,
) -> Result<bool>
where
    E: Event,
    F: Fn(&EventType, &str) -> Option<Arc<E>>,
{
    info!(
        "auth_check beginning for {} ({})",
        incoming_event.event_id(),
        incoming_event.event_type()
    );

    // [synapse] check that all the events are in the same room as `incoming_event`

    // [synapse] do_sig_check check the event has valid signatures for member events

    // TODO do_size_check is false when called by `iterative_auth_check`
    // do_size_check is also mostly accomplished by ruma with the exception of checking event_type,
    // state_key, and json are below a certain size (255 and 65_536 respectively)

    let sender = incoming_event.sender();

    // Implementation of https://matrix.org/docs/spec/rooms/v1#authorization-rules
    //
    // 1. If type is m.room.create:
    if incoming_event.event_type() == EventType::RoomCreate {
        info!("start m.room.create check");

        // If it has any previous events, reject
        if !incoming_event.prev_events().is_empty() {
            warn!("the room creation event had previous events");
            return Ok(false);
        }

        // If the domain of the room_id does not match the domain of the sender, reject
        if incoming_event.room_id().server_name() != sender.server_name() {
            warn!("creation events server does not match sender");
            return Ok(false); // creation events room id does not match senders
        }

        // If content.room_version is present and is not a recognized version, reject
        if serde_json::from_value::<RoomVersionId>(
            incoming_event
                .content()
                .get("room_version")
                .cloned()
                // TODO synapse defaults to version 1
                .unwrap_or_else(|| serde_json::json!("1")),
        )
        .is_err()
        {
            warn!("invalid room version found in m.room.create event");
            return Ok(false);
        }

        // If content has no creator field, reject
        if incoming_event.content().get("creator").is_none() {
            warn!("no creator field found in room create content");
            return Ok(false);
        }

        info!("m.room.create event was allowed");
        return Ok(true);
    }

    /*
    // 2. Reject if auth_events
    // a. auth_events cannot have duplicate keys since it's a BTree
    // b. All entries are valid auth events according to spec
    let expected_auth = auth_types_for_event(
        incoming_event.kind,
        sender,
        incoming_event.state_key,
        incoming_event.content().clone(),
    );

    dbg!(&expected_auth);

    for ev_key in auth_events.keys() {
        // (b)
        if !expected_auth.contains(ev_key) {
            warn!("auth_events contained invalid auth event");
            return Ok(false);
        }
    }
    */

    let room_create_event = fetch_state(&EventType::RoomCreate, "");

    // 3. If event does not have m.room.create in auth_events reject
    if room_create_event.is_none() {
        warn!("no m.room.create event in auth chain");

        return Ok(false);
    }

    // [synapse] checks for federation here

    // 4. If type is m.room.aliases
    if incoming_event.event_type() == EventType::RoomAliases
        && room_version.special_case_aliases_auth
    {
        info!("starting m.room.aliases check");

        // If sender's domain doesn't matches state_key, reject
        if incoming_event.state_key().as_deref() != Some(sender.server_name().as_str()) {
            warn!("state_key does not match sender");
            return Ok(false);
        }

        info!("m.room.aliases event was allowed");
        return Ok(true);
    }

    let power_levels_event = fetch_state(&EventType::RoomPowerLevels, "");
    let sender_member_event = fetch_state(&EventType::RoomMember, sender.as_str());

    if incoming_event.event_type() == EventType::RoomMember {
        info!("starting m.room.member check");
        let state_key = match incoming_event.state_key() {
            None => {
                warn!("no statekey in member event");
                return Ok(false);
            }
            Some(s) => s,
        };

        let membership = incoming_event
            .content()
            .get("membership")
            .map(|m| serde_json::from_value::<MembershipState>(m.clone()));

        if !matches!(membership, Some(Ok(_))) {
            warn!("no valid membership field found for m.room.member event content");
            return Ok(false);
        }

        let target_user =
            UserId::try_from(state_key).map_err(|e| Error::InvalidPdu(format!("{}", e)))?;

        if !valid_membership_change(
            &target_user,
            fetch_state(&EventType::RoomMember, target_user.as_str()),
            sender,
            sender_member_event,
            incoming_event.content(),
            prev_event,
            current_third_party_invite,
            power_levels_event,
            fetch_state(&EventType::RoomJoinRules, ""),
        )? {
            return Ok(false);
        }

        info!("m.room.member event was allowed");
        return Ok(true);
    }

    // If the sender's current membership state is not join, reject
    let mem = match sender_member_event {
        Some(mem) => mem,
        None => {
            warn!("sender not found in room");
            return Ok(false);
        }
    };

    let membership_state = serde_json::from_value::<MembershipState>(
        mem.content()
            .get("membership")
            .expect("we should test before that this field exists")
            .clone(),
    )?;

    if !matches!(membership_state, MembershipState::Join) {
        warn!("sender's membership is not join");
        return Ok(false);
    }

    let sender_power_level = if let Some(pl) = &power_levels_event {
        if let Ok(content) = serde_json::from_value::<PowerLevelsEventContent>(pl.content()) {
            if let Some(level) = content.users.get(sender) {
                *level
            } else {
                content.users_default
            }
        } else {
            int!(0) // TODO if this fails DB error?
        }
    } else {
        // If no power level event found the creator gets 100 everyone else gets 0
        room_create_event
            .and_then(|create| serde_json::from_value::<CreateEventContent>(create.content()).ok())
            .and_then(|create| (create.creator == *sender).then(|| int!(100)))
            .unwrap_or_default()
    };

    // Allow if and only if sender's current power level is greater than
    // or equal to the invite level
    if incoming_event.event_type() == EventType::RoomThirdPartyInvite {
        let invite_level = match &power_levels_event {
            Some(power_levels) => {
                serde_json::from_value::<PowerLevelsEventContent>(power_levels.content())?.invite
            }
            None => int!(50),
        };

        if sender_power_level < invite_level {
            warn!("sender's cannot send invites in this room");
            return Ok(false);
        }
    }

    // If the event type's required power level is greater than the sender's power level, reject
    // If the event has a state_key that starts with an @ and does not match the sender, reject.
    if !can_send_event(incoming_event, power_levels_event.as_ref(), sender_power_level) {
        warn!("user cannot send event");
        return Ok(false);
    }

    if incoming_event.event_type() == EventType::RoomPowerLevels {
        info!("starting m.room.power_levels check");

        if let Some(required_pwr_lvl) = check_power_levels(
            room_version,
            incoming_event,
            power_levels_event.as_ref(),
            sender_power_level,
        ) {
            if !required_pwr_lvl {
                warn!("power level was not allowed");
                return Ok(false);
            }
        } else {
            warn!("power level was not allowed");
            return Ok(false);
        }
        info!("power levels event allowed");
    }

    // Room version 3: Redaction events are always accepted (provided the event is allowed by
    // `events` and `events_default` in the power levels). However, servers should not apply or
    // send redaction's to clients until both the redaction event and original event have been
    // seen, and are valid. Servers should only apply redaction's to events where the sender's
    // domains match, or the sender of the redaction has the appropriate permissions per the
    // power levels.

    if room_version.extra_redaction_checks
        && incoming_event.event_type() == EventType::RoomRedaction
    {
        let default = int!(50);
        let redact_level = if let Some(pl) = power_levels_event {
            // TODO do this the right way and deserialize
            if let Some(level) = pl.content().get("redact") {
                level.to_string().parse().unwrap_or(default)
            } else {
                int!(0)
            }
        } else {
            default
        };

        if !check_redaction(room_version, incoming_event, sender_power_level, redact_level)? {
            return Ok(false);
        }
    }

    info!("allowing event passed all checks");
    Ok(true)
}

// TODO deserializing the member, power, join_rules event contents is done in conduit
// just before this is called. Could they be passed in?
/// Does the user who sent this member event have required power levels to do so.
///
/// * `user` - Information about the membership event and user making the request.
/// * `prev_event` - The event that occurred immediately before the `user` event or None.
/// * `auth_events` - The set of auth events that relate to a membership event.
///
/// This is generated by calling `auth_types_for_event` with the membership event and the current
/// State.
#[allow(clippy::too_many_arguments)]
fn valid_membership_change<E: Event>(
    target_user: &UserId,
    target_user_membership_event: Option<Arc<E>>,
    sender: &UserId,
    sender_membership_event: Option<Arc<E>>,
    content: serde_json::Value,
    prev_event: Option<Arc<E>>,
    current_third_party_invite: Option<Arc<E>>,
    power_levels_event: Option<Arc<E>>,
    join_rules_event: Option<Arc<E>>,
) -> Result<bool> {
    let target_membership = serde_json::from_value::<MembershipState>(
        content.get("membership").expect("we test before that this field exists").clone(),
    )?;

    let third_party_invite = content
        .get("third_party_invite")
        .map(|t| serde_json::from_value::<ThirdPartyInvite>(t.clone()));

    let sender_is_joined = {
        let sender_membership = match &sender_membership_event {
            Some(pdu) => serde_json::from_value(
                pdu.content()
                    .get("membership")
                    .expect("we assume existing events are valid")
                    .clone(),
            )?,
            None => MembershipState::Leave,
        };

        sender_membership == MembershipState::Join
    };

    let target_user_current_membership = match &target_user_membership_event {
        Some(pdu) => serde_json::from_value(
            pdu.content().get("membership").expect("we assume existing events are valid").clone(),
        )?,
        None => MembershipState::Leave,
    };

    let power_levels: PowerLevelsEventContent = match &power_levels_event {
        Some(ev) => serde_json::from_value(ev.content())?,
        None => PowerLevelsEventContent::default(),
    };

    let sender_power = power_levels
        .users
        .get(sender)
        .or_else(|| sender_is_joined.then(|| &power_levels.users_default));

    let target_power = power_levels.users.get(target_user).or_else(|| {
        (target_membership == MembershipState::Join).then(|| &power_levels.users_default)
    });

    let mut join_rules = JoinRule::Invite;
    if let Some(jr) = &join_rules_event {
        join_rules = serde_json::from_value::<JoinRulesEventContent>(jr.content())?.join_rule;
    }

    if let Some(prev) = prev_event {
        if prev.event_type() == EventType::RoomCreate && prev.prev_events().is_empty() {
            return Ok(true);
        }
    }

    let power_levels_event_id = power_levels_event.as_ref().map(|e| e.event_id());
    let sender_membership_event_id = sender_membership_event.as_ref().map(|e| e.event_id());
    let target_user_membership_event_id =
        target_user_membership_event.as_ref().map(|e| e.event_id());

    Ok(match target_membership {
        MembershipState::Join => {
            if sender != target_user {
                warn!("Can't make other user join");
                false
            } else if let MembershipState::Ban = target_user_current_membership {
                warn!(?target_user_membership_event_id, "Banned user can't join");
                false
            } else {
                let allow = join_rules == JoinRule::Invite
                    && (target_user_current_membership == MembershipState::Join
                        || target_user_current_membership == MembershipState::Invite)
                    || join_rules == JoinRule::Public;

                if !allow {
                    warn!(
                        join_rules_event_id = ?join_rules_event.as_ref().map(|e| e.event_id()),
                        ?target_user_membership_event_id,
                        "Can't join if join rules is not public and user is not invited / joined",
                    );
                }
                allow
            }
        }
        MembershipState::Invite => {
            // If content has third_party_invite key
            if let Some(Ok(tp_id)) = third_party_invite {
                if target_user_current_membership == MembershipState::Ban {
                    warn!(?target_user_membership_event_id, "Can't invite banned user");
                    false
                } else {
                    let allow = verify_third_party_invite(
                        Some(target_user),
                        sender,
                        &tp_id,
                        current_third_party_invite,
                    );
                    if !allow {
                        warn!("Third party invite invalid");
                    }
                    allow
                }
            } else if !sender_is_joined
                || target_user_current_membership == MembershipState::Join
                || target_user_current_membership == MembershipState::Ban
            {
                warn!(
                    ?target_user_membership_event_id,
                    ?sender_membership_event_id,
                    "Can't invite user if sender not joined or the user is currently joined or \
                     banned",
                );
                false
            } else {
                let allow = sender_power.filter(|&p| p >= &power_levels.invite).is_some();
                if !allow {
                    warn!(
                        ?target_user_membership_event_id,
                        ?power_levels_event_id,
                        "User does not have enough power to invite",
                    );
                }
                allow
            }
        }
        MembershipState::Leave => {
            if sender == target_user {
                let allow = target_user_current_membership == MembershipState::Join
                    || target_user_current_membership == MembershipState::Invite;
                if !allow {
                    warn!(?target_user_membership_event_id, "Can't leave if not invited or joined");
                }
                allow
            } else if !sender_is_joined
                || target_user_current_membership == MembershipState::Ban
                    && sender_power.filter(|&p| p < &power_levels.ban).is_some()
            {
                warn!(
                    ?target_user_membership_event_id,
                    ?sender_membership_event_id,
                    "Can't kick if sender not joined or user is already banned",
                );
                false
            } else {
                let allow = sender_power.filter(|&p| p >= &power_levels.kick).is_some()
                    && target_power < sender_power;
                if !allow {
                    warn!(
                        ?target_user_membership_event_id,
                        ?power_levels_event_id,
                        "User does not have enough power to kick",
                    );
                }
                allow
            }
        }
        MembershipState::Ban => {
            if !sender_is_joined {
                warn!(?sender_membership_event_id, "Can't ban user if sender is not joined");
                false
            } else {
                let allow = sender_power.filter(|&p| p >= &power_levels.ban).is_some()
                    && target_power < sender_power;
                if !allow {
                    warn!(
                        ?target_user_membership_event_id,
                        ?power_levels_event_id,
                        "User does not have enough power to ban",
                    );
                }
                allow
            }
        }
        _ => {
            warn!("Unknown membership transition");
            false
        }
    })
}

/// Is the user allowed to send a specific event based on the rooms power levels.
///
/// Does the event have the correct userId as its state_key if it's not the "" state_key.
fn can_send_event<E: Event>(event: &Arc<E>, ple: Option<&Arc<E>>, user_level: Int) -> bool {
    let event_type_power_level = get_send_level(&event.event_type(), event.state_key(), ple);

    debug!("{} ev_type {} usr {}", event.event_id(), event_type_power_level, user_level);

    if user_level < event_type_power_level {
        return false;
    }

    if event.state_key().as_ref().map_or(false, |k| k.starts_with('@'))
        && event.state_key().as_deref() != Some(event.sender().as_str())
    {
        return false; // permission required to post in this room
    }

    true
}

/// Confirm that the event sender has the required power levels.
fn check_power_levels<E>(
    room_version: &RoomVersion,
    power_event: &Arc<E>,
    previous_power_event: Option<&Arc<E>>,
    user_level: Int,
) -> Option<bool>
where
    E: Event,
{
    match power_event.state_key().as_deref() {
        Some("") => {}
        Some(key) => {
            error!("m.room.power_levels event has non-empty state key: {}", key);
            return None;
        }
        None => {
            error!("check_power_levels requires an m.room.power_levels *state* event argument");
            return None;
        }
    }

    let current_state = match previous_power_event {
        Some(current_state) => current_state,
        // If there is no previous m.room.power_levels event in the room, allow
        None => return Some(true),
    };

    // If users key in content is not a dictionary with keys that are valid user IDs
    // with values that are integers (or a string that is an integer), reject.
    let user_content =
        serde_json::from_value::<PowerLevelsEventContent>(power_event.content()).unwrap();

    let current_content =
        serde_json::from_value::<PowerLevelsEventContent>(current_state.content()).unwrap();

    // Validation of users is done in Ruma, synapse for loops validating user_ids and integers here
    info!("validation of power event finished");

    let mut user_levels_to_check = BTreeSet::new();
    let old_list = &current_content.users;
    let user_list = &user_content.users;
    for user in old_list.keys().chain(user_list.keys()) {
        let user: &UserId = user;
        user_levels_to_check.insert(user);
    }

    debug!("users to check {:?}", user_levels_to_check);

    let mut event_levels_to_check = BTreeSet::new();
    let old_list = &current_content.events;
    let new_list = &user_content.events;
    for ev_id in old_list.keys().chain(new_list.keys()) {
        let ev_id: &EventType = ev_id;
        event_levels_to_check.insert(ev_id);
    }

    debug!("events to check {:?}", event_levels_to_check);

    let old_state = &current_content;
    let new_state = &user_content;

    // synapse does not have to split up these checks since we can't combine UserIds and
    // EventTypes we do 2 loops

    // UserId loop
    for user in user_levels_to_check {
        let old_level = old_state.users.get(user);
        let new_level = new_state.users.get(user);
        if old_level.is_some() && new_level.is_some() && old_level == new_level {
            continue;
        }

        // If the current value is equal to the sender's current power level, reject
        if user != power_event.sender() && old_level == Some(&user_level) {
            warn!("m.room.power_level cannot remove ops == to own");
            return Some(false); // cannot remove ops level == to own
        }

        // If the current value is higher than the sender's current power level, reject
        // If the new value is higher than the sender's current power level, reject
        let old_level_too_big = old_level > Some(&user_level);
        let new_level_too_big = new_level > Some(&user_level);
        if old_level_too_big || new_level_too_big {
            warn!("m.room.power_level failed to add ops > than own");
            return Some(false); // cannot add ops greater than own
        }
    }

    // EventType loop
    for ev_type in event_levels_to_check {
        let old_level = old_state.events.get(ev_type);
        let new_level = new_state.events.get(ev_type);
        if old_level.is_some() && new_level.is_some() && old_level == new_level {
            continue;
        }

        // If the current value is higher than the sender's current power level, reject
        // If the new value is higher than the sender's current power level, reject
        let old_level_too_big = old_level > Some(&user_level);
        let new_level_too_big = new_level > Some(&user_level);
        if old_level_too_big || new_level_too_big {
            warn!("m.room.power_level failed to add ops > than own");
            return Some(false); // cannot add ops greater than own
        }
    }

    // Notifications, currently there is only @room
    if room_version.limit_notifications_power_levels {
        let old_level = old_state.notifications.room;
        let new_level = new_state.notifications.room;
        if old_level != new_level {
            // If the current value is higher than the sender's current power level, reject
            // If the new value is higher than the sender's current power level, reject
            let old_level_too_big = old_level > user_level;
            let new_level_too_big = new_level > user_level;
            if old_level_too_big || new_level_too_big {
                warn!("m.room.power_level failed to add ops > than own");
                return Some(false); // cannot add ops greater than own
            }
        }
    }

    let levels =
        ["users_default", "events_default", "state_default", "ban", "redact", "kick", "invite"];
    let old_state = serde_json::to_value(old_state).unwrap();
    let new_state = serde_json::to_value(new_state).unwrap();
    for lvl_name in &levels {
        if let Some((old_lvl, new_lvl)) = get_deserialize_levels(&old_state, &new_state, lvl_name) {
            let old_level_too_big = old_lvl > user_level;
            let new_level_too_big = new_lvl > user_level;

            if old_level_too_big || new_level_too_big {
                warn!("cannot add ops > than own");
                return Some(false);
            }
        }
    }

    Some(true)
}

fn get_deserialize_levels(
    old: &serde_json::Value,
    new: &serde_json::Value,
    name: &str,
) -> Option<(Int, Int)> {
    Some((
        serde_json::from_value(old.get(name)?.clone()).ok()?,
        serde_json::from_value(new.get(name)?.clone()).ok()?,
    ))
}

/// Does the event redacting come from a user with enough power to redact the given event.
pub fn check_redaction<E: Event>(
    _room_version: &RoomVersion,
    redaction_event: &Arc<E>,
    user_level: Int,
    redact_level: Int,
) -> Result<bool> {
    if user_level >= redact_level {
        info!("redaction allowed via power levels");
        return Ok(true);
    }

    // If the domain of the event_id of the event being redacted is the same as the
    // domain of the event_id of the m.room.redaction, allow
    if redaction_event.event_id().server_name()
        == redaction_event.redacts().as_ref().and_then(|id| id.server_name())
    {
        info!("redaction event allowed via room version 1 rules");
        return Ok(true);
    }

    Ok(false)
}

/// Check that the member event matches `state`.
///
/// This function returns false instead of failing when deserialization fails.
pub fn check_membership<E: Event>(member_event: Option<Arc<E>>, state: MembershipState) -> bool {
    if let Some(event) = member_event {
        if let Some(Ok(membership)) = event
            .content()
            .get("membership")
            .map(|m| serde_json::from_value::<MembershipState>(m.clone()))
        {
            membership == state
        } else {
            false
        }
    } else {
        false
    }
}

/// Can this room federate based on its m.room.create event.
pub fn can_federate<E: Event>(auth_events: &StateMap<Arc<E>>) -> bool {
    let creation_event = auth_events.get(&(EventType::RoomCreate, "".into()));
    if let Some(ev) = creation_event {
        if let Some(fed) = ev.content().get("m.federate") {
            fed == "true"
        } else {
            false
        }
    } else {
        false
    }
}

/// Helper function to fetch the power level needed to send an event of type
/// `e_type` based on the rooms "m.room.power_level" event.
pub fn get_send_level<E: Event>(
    e_type: &EventType,
    state_key: Option<String>,
    power_lvl: Option<&Arc<E>>,
) -> Int {
    power_lvl
        .and_then(|ple| {
            serde_json::from_value::<PowerLevelsEventContent>(ple.content())
                .map(|content| {
                    content.events.get(e_type).copied().unwrap_or_else(|| {
                        if state_key.is_some() {
                            content.state_default
                        } else {
                            content.events_default
                        }
                    })
                })
                .ok()
        })
        .unwrap_or_else(|| if state_key.is_some() { int!(50) } else { int!(0) })
}

pub fn verify_third_party_invite<E: Event>(
    target_user: Option<&UserId>,
    sender: &UserId,
    tp_id: &ThirdPartyInvite,
    current_third_party_invite: Option<Arc<E>>,
) -> bool {
    // 1. Check for user being banned happens before this is called
    // checking for mxid and token keys is done by ruma when deserializing

    // The state key must match the invitee
    if target_user != Some(&tp_id.signed.mxid) {
        return false;
    }

    // If there is no m.room.third_party_invite event in the current room state
    // with state_key matching token, reject
    if let Some(current_tpid) = current_third_party_invite {
        if current_tpid.state_key().as_ref() != Some(&tp_id.signed.token) {
            return false;
        }

        if sender != current_tpid.sender() {
            return false;
        }

        // If any signature in signed matches any public key in the m.room.third_party_invite event,
        // allow
        if let Ok(tpid_ev) =
            serde_json::from_value::<ThirdPartyInviteEventContent>(current_tpid.content())
        {
            // A list of public keys in the public_keys field
            for key in tpid_ev.public_keys.unwrap_or_default() {
                if key.public_key == tp_id.signed.token {
                    return true;
                }
            }
            // A single public key in the public_key field
            tpid_ev.public_key == tp_id.signed.token
        } else {
            false
        }
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        event_auth::valid_membership_change,
        test_utils::{alice, charlie, event_id, member_content_ban, to_pdu_event, INITIAL_EVENTS},
        StateMap,
    };
    use ruma_events::EventType;

    #[test]
    fn test_ban_pass() {
        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());
        let events = INITIAL_EVENTS();

        let prev_event =
            events.values().find(|ev| ev.event_id().as_str().contains("IMC")).map(Arc::clone);

        let auth_events = events
            .values()
            .map(|ev| ((ev.event_type(), ev.state_key()), Arc::clone(ev)))
            .collect::<StateMap<_>>();

        let requester = to_pdu_event(
            "HELLO",
            alice(),
            EventType::RoomMember,
            Some(charlie().as_str()),
            member_content_ban(),
            &[],
            &[event_id("IMC")],
        );

        let fetch_state = |ty, key| auth_events.get(&(ty, key)).cloned();
        let target_user = charlie();
        let sender = alice();

        assert!(valid_membership_change(
            &target_user,
            fetch_state(EventType::RoomMember, target_user.to_string()),
            &sender,
            fetch_state(EventType::RoomMember, sender.to_string()),
            requester.content(),
            prev_event,
            None,
            fetch_state(EventType::RoomPowerLevels, "".to_owned()),
            fetch_state(EventType::RoomJoinRules, "".to_owned()),
        )
        .unwrap());
    }

    #[test]
    fn test_ban_fail() {
        let _ =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());
        let events = INITIAL_EVENTS();

        let prev_event =
            events.values().find(|ev| ev.event_id().as_str().contains("IMC")).map(Arc::clone);

        let auth_events = events
            .values()
            .map(|ev| ((ev.event_type(), ev.state_key()), Arc::clone(ev)))
            .collect::<StateMap<_>>();

        let requester = to_pdu_event(
            "HELLO",
            charlie(),
            EventType::RoomMember,
            Some(alice().as_str()),
            member_content_ban(),
            &[],
            &[event_id("IMC")],
        );

        let fetch_state = |ty, key| auth_events.get(&(ty, key)).cloned();
        let target_user = alice();
        let sender = charlie();

        assert!(!valid_membership_change(
            &target_user,
            fetch_state(EventType::RoomMember, target_user.to_string()),
            &sender,
            fetch_state(EventType::RoomMember, sender.to_string()),
            requester.content(),
            prev_event,
            None,
            fetch_state(EventType::RoomPowerLevels, "".to_owned()),
            fetch_state(EventType::RoomJoinRules, "".to_owned()),
        )
        .unwrap());
    }
}
