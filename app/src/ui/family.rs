//! Who uses this server and from what: one group per person, their devices
//! under them. Pairing starts from the person it is for, so there is no
//! "whose" to pick, and the token it prints is shown in that group, once.
//!
//! An `admin` device sees everyone and can add people and pair devices. A
//! `play` one sees its own person's devices, can pair more `play` ones and
//! revoke them: a new or lost phone should not need whoever has the admin
//! device to hand.

use crate::api::{Device, Scope, User};
use crate::backend::backend;
use dioxus::prelude::*;

/// Everyone in the family and every device the server will tell this one
/// about, shared so the settings and anything else read the same lists.
#[derive(Clone, Copy)]
pub struct Family {
    pub people: Signal<Vec<User>>,
    pub devices: Signal<Vec<Device>>,
}

impl Family {
    pub fn new() -> Self {
        Self {
            people: Signal::new(Vec::new()),
            devices: Signal::new(Vec::new()),
        }
    }

    fn refresh(self) {
        spawn(async move {
            let mut family = self;
            if let Ok(found) = backend().devices().await {
                family.devices.set(found);
            }
            if let Ok(found) = backend().users().await {
                family.people.set(found);
            }
        });
    }
}

/// A token just minted: whose group it shows in, the device's name, and the
/// token itself.
#[derive(Clone, PartialEq)]
struct Granted {
    person: String,
    device: String,
    token: String,
}

/// One person and their devices, in the order the panel draws them.
struct Group {
    name: String,
    devices: Vec<Device>,
}

/// People first in the order the server lists them, with this device's
/// person on top; then anyone only known from a device, which is all a `play`
/// device ever sees, since it may not list people.
fn groups(people: &[User], devices: &[Device], me: Option<&str>) -> Vec<Group> {
    let mut groups: Vec<Group> = people
        .iter()
        .map(|user| Group {
            name: user.name.clone(),
            devices: Vec::new(),
        })
        .collect();
    for device in devices {
        match groups.iter_mut().find(|group| group.name == device.user) {
            Some(group) => group.devices.push(device.clone()),
            None => groups.push(Group {
                name: device.user.clone(),
                devices: vec![device.clone()],
            }),
        }
    }
    if let Some(me) = me {
        if let Some(at) = groups.iter().position(|group| group.name == me) {
            let mine = groups.remove(at);
            groups.insert(0, mine);
        }
    }
    groups
}

#[component]
pub fn FamilyRow() -> Element {
    let family = use_context::<Family>();
    let mut people = family.people;
    let me = use_context::<super::Me>().0;

    // Which group's pairing form is open, by person.
    let mut pairing = use_signal(|| None::<String>);
    let mut adding = use_signal(|| false);
    let mut new_person = use_signal(String::new);
    let granted = use_signal(|| None::<Granted>);
    let mut status = use_signal(|| None::<String>);

    use_future(move || async move { family.refresh() });

    // Unknown until the server answers, and then for good on one older than
    // scopes had a name, where pairing was the only thing this panel was for.
    let admin = me.read().as_ref().is_none_or(|device| device.scope == Scope::Admin);
    let mine = me.read().as_ref().map(|device| device.user.clone());
    let this_device = me.read().as_ref().map(|device| device.id);
    let all = groups(&people.read(), &family.devices.read(), mine.as_deref());
    // Servers older than users name nobody; there is one group and no one to
    // add.
    let has_people = all.iter().any(|group| !group.name.is_empty());

    let add_person = move || {
        let wanted = new_person.peek().trim().to_string();
        if wanted.is_empty() {
            return;
        }
        spawn(async move {
            match backend().add_user(&wanted).await {
                Ok(user) => {
                    status.set(None);
                    new_person.set(String::new());
                    adding.set(false);
                    if let Ok(found) = backend().users().await {
                        people.set(found);
                    }
                    // A person without a device cannot do anything yet.
                    pairing.set(Some(user.name));
                }
                Err(err) => status.set(Some(format!("{err:#}"))),
            }
        });
    };

    rsx! {
        section { class: "panel setting family",
            h2 {
                if admin && has_people { "People & devices" } else { "Devices" }
                span { class: "spacer" }
                if admin && has_people {
                    button {
                        class: if adding() { "chip active" } else { "chip" },
                        onclick: move |_| adding.set(!adding()),
                        "+ person"
                    }
                }
            }
            p { class: "muted",
                if admin && has_people {
                    "Each person has their own devices, queue and likes; the music account and the space are everyone's. "
                }
                "Every device has its own token, so one can be revoked without re-pairing the rest."
            }

            if adding() {
                div { class: "family-add",
                    input {
                        class: "search",
                        placeholder: "their name, e.g. sam",
                        value: "{new_person}",
                        autofocus: true,
                        oninput: move |event| new_person.set(event.value()),
                        onkeydown: move |event| {
                            if event.key() == Key::Enter {
                                add_person();
                            } else if event.key() == Key::Escape {
                                adding.set(false);
                            }
                        },
                    }
                    button { onclick: move |_| adding.set(false), "Cancel" }
                    button {
                        class: "primary",
                        disabled: new_person.read().trim().is_empty(),
                        onclick: move |_| add_person(),
                        "Add"
                    }
                }
            }

            if let Some(message) = status.read().clone() {
                p { class: "error", "{message}" }
            }

            div { class: "people",
                for group in all {
                    PersonGroup {
                        key: "{group.name}",
                        name: group.name.clone(),
                        devices: group.devices,
                        you: mine.as_deref() == Some(group.name.as_str()),
                        this_device,
                        admin,
                        pairing,
                        granted,
                        status,
                    }
                }
            }
        }
    }
}

#[component]
fn PersonGroup(
    name: String,
    devices: Vec<Device>,
    you: bool,
    this_device: Option<i64>,
    admin: bool,
    pairing: Signal<Option<String>>,
    granted: Signal<Option<Granted>>,
    status: Signal<Option<String>>,
) -> Element {
    let family = use_context::<Family>();
    // The device whose revoke has been pressed once and waits for a second.
    let mut arming = use_signal(|| None::<i64>);

    let open = pairing.read().as_deref() == Some(name.as_str());
    let token = granted.read().clone().filter(|grant| grant.person == name);
    let count = match devices.len() {
        0 => "no devices".to_string(),
        1 => "1 device".to_string(),
        n => format!("{n} devices"),
    };
    let toggle = name.clone();

    rsx! {
        div { class: "person",
            div { class: "person-head",
                Avatar { name: name.clone() }
                div { class: "person-text",
                    span { class: "person-name",
                        if name.is_empty() { "Everyone" } else { "{name}" }
                        if you {
                            span { class: "muted", " · you" }
                        }
                    }
                    span { class: "muted", "{count}" }
                }
                span { class: "spacer" }
                if admin || you {
                    button {
                        class: if open { "chip active" } else { "chip" },
                        onclick: move |_| {
                            let next = (!open).then(|| toggle.clone());
                            pairing.set(next);
                        },
                        "+ device"
                    }
                }
            }

            if open {
                PairForm { person: name.clone(), admin, pairing, granted, status }
            }

            if let Some(grant) = token {
                TokenCard { grant, granted }
            }

            if !devices.is_empty() {
                ul { class: "devices",
                    for device in devices {
                        li {
                            key: "{device.id}",
                            class: "device",
                            div { class: "device-text",
                                span { class: "device-name",
                                    "{device.name}"
                                    if this_device == Some(device.id) {
                                        span { class: "this-device", "this device" }
                                    }
                                }
                                span {
                                    class: "muted",
                                    title: device.last_seen.clone().unwrap_or_default(),
                                    {seen(device.last_seen.as_deref())}
                                }
                            }
                            span { class: "access {device.scope.as_str()}", "{device.scope.as_str()}" }
                            button {
                                class: if *arming.read() == Some(device.id) { "chip danger armed" } else { "chip danger" },
                                onclick: move |_| {
                                    let id = device.id;
                                    if *arming.peek() != Some(id) {
                                        arming.set(Some(id));
                                        return;
                                    }
                                    arming.set(None);
                                    spawn(async move {
                                        let mut status = status;
                                        match backend().revoke_device(id).await {
                                            Ok(()) => status.set(None),
                                            Err(err) => status.set(Some(format!("{err:#}"))),
                                        }
                                        family.refresh();
                                    });
                                },
                                if *arming.read() == Some(device.id) { "confirm revoke" } else { "revoke" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// An initial on a colour picked from the name, so the same person is the
/// same colour every time without anyone choosing one.
#[component]
fn Avatar(name: String) -> Element {
    let initial = name
        .chars()
        .next()
        .map(|first| first.to_uppercase().to_string())
        .unwrap_or_else(|| "·".into());
    let hue = name.bytes().fold(0u32, |hash, byte| hash.wrapping_mul(31).wrapping_add(byte as u32)) % 360;
    rsx! {
        span { class: "avatar", style: "background: hsl({hue}, 40%, 34%);", "{initial}" }
    }
}

#[component]
fn PairForm(
    person: String,
    admin: bool,
    pairing: Signal<Option<String>>,
    granted: Signal<Option<Granted>>,
    status: Signal<Option<String>>,
) -> Element {
    let family = use_context::<Family>();
    let mut name = use_signal(String::new);
    let mut scope = use_signal(|| Scope::Play);
    let mut busy = use_signal(|| false);
    let whose = person.clone();

    let mut pair = move || {
        let wanted = name.peek().trim().to_string();
        if wanted.is_empty() || *busy.peek() {
            return;
        }
        let chosen = *scope.peek();
        let whose = whose.clone();
        busy.set(true);
        spawn(async move {
            let user = Some(whose.as_str()).filter(|whose| !whose.is_empty());
            match backend().pair_device(&wanted, chosen, user).await {
                Ok(grant) => {
                    status.set(None);
                    granted.set(Some(Granted {
                        person: whose.clone(),
                        device: grant.device.name,
                        token: grant.token,
                    }));
                    pairing.set(None);
                    family.refresh();
                }
                Err(err) => status.set(Some(format!("{err:#}"))),
            }
            busy.set(false);
        });
    };
    let mut submit = pair.clone();

    let hint = match scope() {
        Scope::Play => "Browse, play and like. Can pair and revoke this person's own devices.",
        Scope::Admin => "Everything play can, plus the pipeline, hidden artists, people and pairing.",
    };

    rsx! {
        div { class: "pair-form",
            input {
                class: "search",
                placeholder: "device name, e.g. phone",
                value: "{name}",
                autofocus: true,
                oninput: move |event| name.set(event.value()),
                onkeydown: move |event| {
                    if event.key() == Key::Enter {
                        submit();
                    } else if event.key() == Key::Escape {
                        pairing.set(None);
                    }
                },
            }
            // A play device can only pair play ones; the server says so too.
            if admin {
                div { class: "segmented", role: "radiogroup",
                    for (option, label) in [(Scope::Play, "Play"), (Scope::Admin, "Admin")] {
                        button {
                            key: "{label}",
                            role: "radio",
                            aria_checked: scope() == option,
                            class: if scope() == option { "active" } else { "" },
                            onclick: move |_| scope.set(option),
                            "{label}"
                        }
                    }
                }
            }
            p { class: "muted pair-hint", "{hint}" }
            div { class: "pair-actions",
                button { onclick: move |_| pairing.set(None), "Cancel" }
                button {
                    class: "primary",
                    disabled: name.read().trim().is_empty() || busy(),
                    onclick: move |_| pair(),
                    if busy() { "Pairing…" } else { "Pair" }
                }
            }
        }
    }
}

/// The token, right after pairing, the one time it exists outside the
/// device it is for.
#[component]
fn TokenCard(grant: Granted, granted: Signal<Option<Granted>>) -> Element {
    let mut copied = use_signal(|| false);
    let token = grant.token.clone();

    rsx! {
        div { class: "token-card",
            p {
                "Token for “{grant.device}”. "
                span { class: "muted",
                    "Paste it into Stelly's connect screen on that device, or set it as STELLY_TOKEN. It is not shown again."
                }
            }
            div { class: "token-line",
                code { class: "token", "{grant.token}" }
                button {
                    onclick: move |_| {
                        copy(&token);
                        copied.set(true);
                    },
                    if copied() { "Copied" } else { "Copy" }
                }
                button { class: "primary", onclick: move |_| granted.set(None), "Done" }
            }
        }
    }
}

/// Onto the clipboard, through the webview. The async clipboard API is
/// missing from some webviews, where the old `execCommand` still works.
fn copy(text: &str) {
    let text = serde_json::to_string(text).unwrap_or_default();
    document::eval(&format!(
        "(function (text) {{
            function fallback() {{
                const area = document.createElement('textarea');
                area.value = text;
                document.body.appendChild(area);
                area.select();
                document.execCommand('copy');
                area.remove();
            }}
            if (navigator.clipboard && navigator.clipboard.writeText) {{
                navigator.clipboard.writeText(text).catch(fallback);
            }} else {{
                fallback();
            }}
        }})({text});"
    ));
}

/// When a device last spoke to the server, as a person would say it. The
/// server writes `YYYY-MM-DDTHH:MM:SS+00:00`, always UTC.
fn seen(stamp: Option<&str>) -> String {
    let Some(stamp) = stamp else {
        return "never seen".into();
    };
    let Some(then) = parse_utc(stamp) else {
        return format!("seen {stamp}");
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(then);
    let ago = (now - then).max(0);
    match ago {
        0..60 => "seen just now".into(),
        60..3600 => format!("seen {} min ago", ago / 60),
        3600..86_400 => format!("seen {} h ago", ago / 3600),
        86_400..172_800 => "seen yesterday".into(),
        172_800..2_592_000 => format!("seen {} days ago", ago / 86_400),
        _ => format!("seen {}", &stamp[..10]),
    }
}

fn parse_utc(stamp: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| stamp.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days since 1970-01-01, Howard Hinnant's algorithm, the inverse of the
/// server's `civil_from_days`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_servers_stamps() {
        assert_eq!(parse_utc("1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(parse_utc("2026-10-03T10:18:59+00:00"), Some(1_791_022_739));
        assert_eq!(parse_utc("2024-02-29T00:00:00+00:00"), Some(1_709_164_800));
        assert_eq!(parse_utc("yesterday"), None);
    }
}
