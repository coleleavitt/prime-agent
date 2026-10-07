//! The real accessibility bus: AT-SPI over zbus's blocking API.
//!
//! The bus address comes from `AT_SPI_BUS_ADDRESS` or the session bus's
//! `org.a11y.Bus.GetAddress`; the connection is made once and kept (a
//! failure is retried on the next call). Every method call is bounded by
//! the same 1.5 s timeout libatspi used.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use atspi_proxies::accessible::AccessibleProxyBlocking;
use atspi_proxies::action::ActionProxyBlocking;
use atspi_proxies::bus::BusProxyBlocking;
use atspi_proxies::common::{ObjectRefOwned, Role, State};
use atspi_proxies::component::ComponentProxyBlocking;
use atspi_proxies::editable_text::EditableTextProxyBlocking;
use atspi_proxies::text::TextProxyBlocking;
use atspi_proxies::value::ValueProxyBlocking;
use atspi_proxies::{CoordType, Interface};
use zbus::blocking::Connection;
use zbus::proxy::CacheProperties;

use super::atspi::{AtSpi, States};

const METHOD_TIMEOUT: Duration = Duration::from_millis(1500);
const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";

/// One accessible object on the bus: its owner and path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BusNode {
    name: String,
    path: String,
}

impl BusNode {
    fn from_ref(object: &ObjectRefOwned) -> Option<Self> {
        if object.is_null() {
            return None;
        }
        Some(Self {
            name: object.name_as_str()?.to_string(),
            path: object.path_as_str().to_string(),
        })
    }
}

/// The accessibility bus connection.
#[derive(Default)]
pub(crate) struct BusAtSpi {
    connection: Mutex<Option<Connection>>,
}

/// Build one blocking proxy of `$proxy` for a node.
macro_rules! proxy {
    ($self:ident, $proxy:ident, $node:expr) => {{
        let connection = $self.connection().ok()?;
        $proxy::builder(&connection)
            .destination($node.name.clone())
            .ok()?
            .path($node.path.clone())
            .ok()?
            .cache_properties(CacheProperties::No)
            .build()
            .ok()?
    }};
}

impl BusAtSpi {
    fn connection(&self) -> Result<Connection, String> {
        let mut cached = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(connection) = cached.as_ref() {
            return Ok(connection.clone());
        }
        let connection = connect().map_err(|error| {
            format!(
                "computer use backend unavailable: the AT-SPI accessibility bus is not reachable \
                 ({error}); at-spi2-core must be installed and running in the session"
            )
        })?;
        *cached = Some(connection.clone());
        Ok(connection)
    }

    fn interfaces(&self, node: &BusNode) -> Option<atspi_proxies::InterfaceSet> {
        let accessible = proxy!(self, AccessibleProxyBlocking, node);
        accessible.get_interfaces().ok()
    }

    fn has(&self, node: &BusNode, interface: Interface) -> bool {
        self.interfaces(node)
            .is_some_and(|set| set.contains(interface))
    }

    fn accessible(&self, node: &BusNode) -> Option<AccessibleProxyBlocking<'static>> {
        Some(proxy!(self, AccessibleProxyBlocking, node))
    }
}

fn connect() -> zbus::Result<Connection> {
    let address = match std::env::var("AT_SPI_BUS_ADDRESS") {
        Ok(address) if !address.is_empty() => address,
        _ => {
            let session = Connection::session()?;
            BusProxyBlocking::new(&session)?.get_address()?
        }
    };
    zbus::blocking::connection::Builder::address(address.as_str())?
        .method_timeout(METHOD_TIMEOUT)
        .build()
}

fn failed(error: &zbus::Error) -> String {
    error.to_string()
}

fn int(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

impl AtSpi for BusAtSpi {
    type Node = BusNode;

    fn available(&self) -> Result<(), String> {
        self.connection().map(drop)
    }

    fn applications(&self) -> Option<Vec<BusNode>> {
        let root = BusNode {
            name: REGISTRY.to_string(),
            path: ROOT.to_string(),
        };
        let children = self.accessible(&root)?.get_children().ok()?;
        Some(children.iter().filter_map(BusNode::from_ref).collect())
    }

    fn process_id(&self, node: &BusNode) -> Option<i64> {
        let connection = self.connection().ok()?;
        let dbus = zbus::blocking::fdo::DBusProxy::new(&connection).ok()?;
        let name = zbus::names::BusName::try_from(node.name.as_str()).ok()?;
        dbus.get_connection_unix_process_id(name)
            .ok()
            .map(i64::from)
    }

    fn child_count(&self, node: &BusNode) -> Option<usize> {
        usize::try_from(self.accessible(node)?.child_count().ok()?).ok()
    }

    fn child(&self, node: &BusNode, index: usize) -> Option<BusNode> {
        let index = i32::try_from(index).ok()?;
        BusNode::from_ref(&self.accessible(node)?.get_child_at_index(index).ok()?)
    }

    fn states(&self, node: &BusNode) -> Option<States> {
        let set = self.accessible(node)?.get_state().ok()?;
        Some(States {
            showing: set.contains(State::Showing),
            focused: set.contains(State::Focused),
            active: set.contains(State::Active),
            editable: set.contains(State::Editable),
        })
    }

    fn is_password(&self, node: &BusNode) -> Option<bool> {
        Some(self.accessible(node)?.get_role().ok()? == Role::PasswordText)
    }

    fn role_name(&self, node: &BusNode) -> Option<String> {
        self.accessible(node)?.get_role_name().ok()
    }

    fn name(&self, node: &BusNode) -> Option<String> {
        self.accessible(node)?.name().ok()
    }

    fn description(&self, node: &BusNode) -> Option<String> {
        self.accessible(node)?.description().ok()
    }

    fn has_text(&self, node: &BusNode) -> bool {
        self.has(node, Interface::Text)
    }

    fn character_count(&self, node: &BusNode) -> Option<i64> {
        let text = proxy!(self, TextProxyBlocking, node);
        text.character_count().ok().map(i64::from)
    }

    fn text(&self, node: &BusNode, start: i64, end: i64) -> Option<String> {
        let text = proxy!(self, TextProxyBlocking, node);
        text.get_text(int(start), int(end)).ok()
    }

    fn current_value(&self, node: &BusNode) -> Option<f64> {
        if !self.has(node, Interface::Value) {
            return None;
        }
        let value = proxy!(self, ValueProxyBlocking, node);
        value.current_value().ok()
    }

    fn action_count(&self, node: &BusNode) -> Option<usize> {
        if !self.has(node, Interface::Action) {
            return None;
        }
        let action = proxy!(self, ActionProxyBlocking, node);
        usize::try_from(action.n_actions().ok()?).ok()
    }

    fn action_name(&self, node: &BusNode, index: usize) -> Option<String> {
        let action = proxy!(self, ActionProxyBlocking, node);
        action.get_name(i32::try_from(index).ok()?).ok()
    }

    fn do_action(&self, node: &BusNode, index: usize) -> Result<bool, String> {
        let perform = || -> Option<zbus::Result<bool>> {
            let action = proxy!(self, ActionProxyBlocking, node);
            Some(action.do_action(i32::try_from(index).ok()?))
        };
        match perform() {
            Some(result) => result.map_err(|error| failed(&error)),
            None => Err("the accessibility bus is not reachable".to_string()),
        }
    }

    fn extents(&self, node: &BusNode) -> Option<(i32, i32, i32, i32)> {
        if !self.has(node, Interface::Component) {
            return None;
        }
        let component = proxy!(self, ComponentProxyBlocking, node);
        component.get_extents(CoordType::Window).ok()
    }

    fn grab_focus(&self, node: &BusNode) -> Option<bool> {
        if !self.has(node, Interface::Component) {
            return None;
        }
        let component = proxy!(self, ComponentProxyBlocking, node);
        component.grab_focus().ok()
    }

    fn has_editable_text(&self, node: &BusNode) -> bool {
        self.has(node, Interface::EditableText)
    }

    fn set_text_contents(&self, node: &BusNode, value: &str) -> Result<bool, String> {
        let write = || -> Option<zbus::Result<bool>> {
            let editable = proxy!(self, EditableTextProxyBlocking, node);
            Some(editable.set_text_contents(value))
        };
        match write() {
            Some(result) => result.map_err(|error| failed(&error)),
            None => Err("the accessibility bus is not reachable".to_string()),
        }
    }

    fn selection_count(&self, node: &BusNode) -> Option<i64> {
        let text = proxy!(self, TextProxyBlocking, node);
        text.get_n_selections().ok().map(i64::from)
    }

    fn set_selection(&self, node: &BusNode, start: i64, end: i64) -> Result<bool, String> {
        let select = || -> Option<zbus::Result<bool>> {
            let text = proxy!(self, TextProxyBlocking, node);
            Some(text.set_selection(0, int(start), int(end)))
        };
        match select() {
            Some(result) => result.map_err(|error| failed(&error)),
            None => Err("the accessibility bus is not reachable".to_string()),
        }
    }

    fn add_selection(&self, node: &BusNode, start: i64, end: i64) -> Result<bool, String> {
        let select = || -> Option<zbus::Result<bool>> {
            let text = proxy!(self, TextProxyBlocking, node);
            Some(text.add_selection(int(start), int(end)))
        };
        match select() {
            Some(result) => result.map_err(|error| failed(&error)),
            None => Err("the accessibility bus is not reachable".to_string()),
        }
    }
}
