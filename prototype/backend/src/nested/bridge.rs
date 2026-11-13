//! When a selection crosses between the parent and the inside: the
//! decisions of the nested backend's selection bridge, apart from Wayland.
//!
//! One [`SelectionBridge`] per selection (clipboard, primary). It is fed
//! what happens (a client inside copied, the parent announced a selection,
//! the parent cancelled one of our sources, the parent's keyboard focus
//! came or went) and answers with what to do ([`Action`]).
//!
//! The parent echoes: a client that sets the selection is sent it back as
//! a new offer, and again with every keyboard focus it gets. Taking that
//! back inside would replace the selection of the client inside with a
//! copy of itself. So while one of our sources is the parent's selection,
//! or is about to be, what the parent announces is not taken.
//!
//! - **Pending → owned.** A source set in the parent is followed by a sync.
//!   The parent sends the echo before the sync's answer when it took the
//!   source (it sends the new selection to the client with the keyboard
//!   focus, which is the one that set it). No echo by then: the parent
//!   did not take it (the focus moved away in between, or a parent that
//!   checks serials refused it); the selection waits for the next focus.
//! - **Owned → idle** when the parent cancels the source: something else
//!   is the parent's selection now. The parent cancels before it
//!   announces the new selection, and the events of one connection arrive
//!   in order, so the new one is taken.
//! - **Waiting.** Without the parent's keyboard focus a selection cannot be
//!   set there. What is copied inside meanwhile waits and is set at the
//!   next focus, with its serial. Whatever the parent announces while it
//!   waits is not taken: the copy inside is the newer one as far as the
//!   nested backend can tell.

/// What the bridge asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    /// Make a source with these types in the parent, set it as the
    /// selection with the latest input serial, and follow it with a sync
    /// whose answer is [`SelectionBridge::confirmed`] with `token`.
    SetParent { token: u64, mime_types: Vec<String> },
    /// Destroy the source `token`: the parent drops its selection if that
    /// source still is it.
    Withdraw { token: u64 },
    /// Offer the parent's selection (the offer it came with) inside, as
    /// `generation`.
    SetInner {
        generation: u64,
        mime_types: Vec<String>,
    },
    /// The parent has no selection: neither has the inside, if what it has
    /// came from the parent.
    ClearInner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outgoing {
    /// Nothing of the inside is, or is to be, the parent's selection.
    Idle,
    /// Copied inside while the parent's keyboard focus was elsewhere.
    Waiting(Vec<String>),
    /// Set in the parent; whether it took it is not known yet.
    Pending {
        token: u64,
        mime_types: Vec<String>,
        echoed: bool,
        /// The focus went away in between.
        left: bool,
    },
    /// The parent's selection is our source.
    Owned { token: u64 },
}

/// See the module documentation.
#[derive(Debug, Clone)]
pub(super) struct SelectionBridge {
    to_parent: bool,
    from_parent: bool,
    outgoing: Outgoing,
    focused: bool,
    next_token: u64,
    /// The generation of the parent's selection last taken inside; 0 for
    /// none.
    generation: u64,
    /// The types of the parent's selection last taken inside.
    imported: Option<Vec<String>>,
    /// A client inside copied after that, and it stayed inside (nothing
    /// goes to the parent).
    inner_newer: bool,
}

impl SelectionBridge {
    pub fn new(to_parent: bool, from_parent: bool) -> Self {
        Self {
            to_parent,
            from_parent,
            outgoing: Outgoing::Idle,
            focused: false,
            next_token: 1,
            generation: 0,
            imported: None,
            inner_newer: false,
        }
    }

    fn token(&mut self) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        token
    }

    /// The source set in the parent, taken or not yet known.
    fn live_token(&self) -> Option<u64> {
        match self.outgoing {
            Outgoing::Pending { token, .. } | Outgoing::Owned { token } => Some(token),
            _ => None,
        }
    }

    /// Whether reads of source `token` are to be answered.
    pub fn is_current(&self, token: u64) -> bool {
        self.live_token() == Some(token)
    }

    /// The generation of the parent's selection that is offered inside.
    pub fn generation(&self) -> Option<u64> {
        self.imported.is_some().then_some(self.generation)
    }

    /// A client inside set the selection (`Some`), or cleared it, or it went
    /// away with its owner (`None`). Never called for what the bridge put
    /// inside itself.
    pub fn inner_changed(&mut self, mime_types: Option<Vec<String>>) -> Vec<Action> {
        if !self.to_parent {
            // What the parent last announced is kept, to tell it from a
            // new selection when the parent sends it again.
            self.inner_newer = mime_types.is_some();
            return Vec::new();
        }
        let mut actions = Vec::new();
        // Whatever the inside had from the parent is replaced either way.
        self.imported = None;
        let live = self.live_token();
        match mime_types {
            Some(mime_types) if self.focused => {
                // The parent replaces (and cancels) a source of ours that
                // is its selection; it need not be withdrawn.
                let token = self.token();
                self.outgoing = Outgoing::Pending {
                    token,
                    mime_types: mime_types.clone(),
                    echoed: false,
                    left: false,
                };
                actions.push(Action::SetParent { token, mime_types });
            }
            Some(mime_types) => {
                // What the parent has of ours is stale until the focus is
                // back.
                if let Some(token) = live {
                    actions.push(Action::Withdraw { token });
                }
                self.outgoing = Outgoing::Waiting(mime_types);
            }
            None => {
                if let Some(token) = live {
                    actions.push(Action::Withdraw { token });
                }
                self.outgoing = Outgoing::Idle;
            }
        }
        actions
    }

    /// The parent announced its selection (`Some` with its types) or that
    /// there is none. The offer is to be kept when the answer has
    /// [`Action::SetInner`], and destroyed otherwise.
    pub fn parent_changed(&mut self, mime_types: Option<Vec<String>>) -> Vec<Action> {
        match &mut self.outgoing {
            Outgoing::Waiting(_) | Outgoing::Owned { .. } => return Vec::new(),
            Outgoing::Pending {
                mime_types: ours,
                echoed,
                ..
            } => {
                if mime_types.as_ref() == Some(ours) {
                    *echoed = true;
                }
                return Vec::new();
            }
            Outgoing::Idle => {}
        }
        if !self.from_parent {
            return Vec::new();
        }
        // Sent again with the focus, while what a client inside copied
        // since stays inside: not a new selection of the parent.
        if self.inner_newer && mime_types.is_some() && mime_types == self.imported {
            return Vec::new();
        }
        self.inner_newer = false;
        match mime_types {
            Some(mime_types) => {
                self.generation += 1;
                self.imported = Some(mime_types.clone());
                vec![Action::SetInner {
                    generation: self.generation,
                    mime_types,
                }]
            }
            None => {
                self.imported = None;
                vec![Action::ClearInner]
            }
        }
    }

    /// The parent cancelled source `token`: something else became its
    /// selection. The caller destroys the source in any case.
    pub fn cancelled(&mut self, token: u64) -> Vec<Action> {
        if self.live_token() == Some(token) {
            self.outgoing = Outgoing::Idle;
        }
        Vec::new()
    }

    /// The answer to the sync that followed [`Action::SetParent`] with
    /// `token` arrived.
    pub fn confirmed(&mut self, token: u64) -> Vec<Action> {
        let Outgoing::Pending {
            token: pending,
            mime_types,
            echoed,
            left,
        } = &self.outgoing
        else {
            return Vec::new();
        };
        if *pending != token {
            return Vec::new();
        }
        if *echoed {
            self.outgoing = Outgoing::Owned { token };
            return Vec::new();
        }
        // The parent did not take it. Again at the next focus, or now if
        // the focus went and came back in between (the likely reason).
        // Not again at once otherwise: a parent that refuses would be
        // asked over and over.
        let retry = *left && self.focused;
        self.outgoing = Outgoing::Waiting(mime_types.clone());
        let mut actions = vec![Action::Withdraw { token }];
        if retry {
            self.focused = false;
            actions.extend(self.focus(true));
        }
        actions
    }

    /// The window got (`true`) or lost the parent's keyboard focus.
    pub fn focus(&mut self, focused: bool) -> Vec<Action> {
        let was = self.focused;
        self.focused = focused;
        if !focused {
            if let Outgoing::Pending { left, .. } = &mut self.outgoing {
                *left = true;
            }
            return Vec::new();
        }
        if was {
            return Vec::new();
        }
        let Outgoing::Waiting(mime_types) = &self.outgoing else {
            return Vec::new();
        };
        let mime_types = mime_types.clone();
        let token = self.token();
        self.outgoing = Outgoing::Pending {
            token,
            mime_types: mime_types.clone(),
            echoed: false,
            left: false,
        };
        vec![Action::SetParent { token, mime_types }]
    }

    /// For the state dump.
    pub fn describe(&self) -> serde_json::Value {
        let (outgoing, waiting) = match &self.outgoing {
            Outgoing::Idle => ("none", None),
            Outgoing::Waiting(types) => ("waiting", Some(types)),
            Outgoing::Pending { mime_types, .. } => ("pending", Some(mime_types)),
            Outgoing::Owned { .. } => ("owned", None),
        };
        serde_json::json!({
            "to_parent": outgoing,
            "to_parent_types": waiting,
            "from_parent_generation": self.generation(),
            "from_parent_types": self.imported,
            "focused": self.focused,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn set_parent(actions: &[Action]) -> u64 {
        match actions {
            [Action::SetParent { token, .. }] => *token,
            other => panic!("expected one SetParent, got {other:?}"),
        }
    }

    fn focused() -> SelectionBridge {
        let mut bridge = SelectionBridge::new(true, true);
        assert!(bridge.focus(true).is_empty());
        bridge
    }

    #[test]
    fn a_copy_inside_goes_out_and_its_echo_stays_out() {
        let mut bridge = focused();
        let text = types(&["text/plain;charset=utf-8", "text/plain"]);
        let token = set_parent(&bridge.inner_changed(Some(text.clone())));
        // The echo, before the sync's answer.
        assert!(bridge.parent_changed(Some(text.clone())).is_empty());
        assert!(bridge.confirmed(token).is_empty());
        assert!(bridge.is_current(token));
        // Sent again with every focus: still ours.
        bridge.focus(false);
        bridge.focus(true);
        assert!(bridge.parent_changed(Some(text)).is_empty());
        assert_eq!(bridge.generation(), None);
    }

    #[test]
    fn a_copy_outside_comes_in_after_the_cancel() {
        let mut bridge = focused();
        let token = set_parent(&bridge.inner_changed(Some(types(&["text/plain"]))));
        bridge.parent_changed(Some(types(&["text/plain"])));
        bridge.confirmed(token);
        assert!(bridge.cancelled(token).is_empty());
        assert!(!bridge.is_current(token));
        let png = types(&["image/png"]);
        assert_eq!(
            bridge.parent_changed(Some(png.clone())),
            vec![Action::SetInner {
                generation: 1,
                mime_types: png
            }]
        );
        assert_eq!(bridge.generation(), Some(1));
        // The parent drops it: so does the inside.
        assert_eq!(bridge.parent_changed(None), vec![Action::ClearInner]);
        assert_eq!(bridge.generation(), None);
    }

    #[test]
    fn two_copies_in_a_row_keep_the_second() {
        let mut bridge = focused();
        let first = set_parent(&bridge.inner_changed(Some(types(&["a"]))));
        let second = set_parent(&bridge.inner_changed(Some(types(&["b"]))));
        // The parent took the first, then the second (cancelling the first).
        assert!(bridge.parent_changed(Some(types(&["a"]))).is_empty());
        assert!(bridge.confirmed(first).is_empty());
        assert!(bridge.cancelled(first).is_empty());
        assert!(bridge.parent_changed(Some(types(&["b"]))).is_empty());
        assert!(bridge.confirmed(second).is_empty());
        assert!(bridge.is_current(second));
        assert!(!bridge.is_current(first));
        // An announcement now is the echo of the second.
        assert!(bridge.parent_changed(Some(types(&["b"]))).is_empty());
    }

    #[test]
    fn a_copy_without_the_focus_waits_for_it() {
        let mut bridge = SelectionBridge::new(true, true);
        assert!(
            bridge
                .inner_changed(Some(types(&["text/plain"])))
                .is_empty()
        );
        // The parent's old selection, before the focus (the order of the
        // specification): not taken.
        assert!(bridge.parent_changed(Some(types(&["old"]))).is_empty());
        let token = set_parent(&bridge.focus(true));
        // The parent's old selection after the focus (the order of Smithay
        // compositors), then the echo: neither is taken.
        assert!(bridge.parent_changed(Some(types(&["old"]))).is_empty());
        assert!(
            bridge
                .parent_changed(Some(types(&["text/plain"])))
                .is_empty()
        );
        assert!(bridge.confirmed(token).is_empty());
        assert!(bridge.is_current(token));
        assert_eq!(bridge.generation(), None);
    }

    #[test]
    fn a_source_the_parent_did_not_take_waits_again() {
        let mut bridge = focused();
        let token = set_parent(&bridge.inner_changed(Some(types(&["text/plain"]))));
        // The focus left before the parent saw the request: no echo.
        bridge.focus(false);
        assert_eq!(bridge.confirmed(token), vec![Action::Withdraw { token }]);
        assert!(!bridge.is_current(token));
        // The parent's selection meanwhile is not taken; the copy is set
        // again with the focus.
        assert!(bridge.parent_changed(Some(types(&["other"]))).is_empty());
        let again = set_parent(&bridge.focus(true));
        assert_ne!(again, token);
    }

    #[test]
    fn a_source_refused_while_the_focus_went_and_came_back_is_set_again() {
        let mut bridge = focused();
        let token = set_parent(&bridge.inner_changed(Some(types(&["a"]))));
        bridge.focus(false);
        assert!(bridge.focus(true).is_empty());
        let actions = bridge.confirmed(token);
        assert_eq!(actions[0], Action::Withdraw { token });
        assert!(matches!(actions[1], Action::SetParent { token: again, .. } if again != token));
    }

    #[test]
    fn a_parent_that_refuses_is_not_asked_over_and_over() {
        let mut bridge = focused();
        let token = set_parent(&bridge.inner_changed(Some(types(&["a"]))));
        assert_eq!(bridge.confirmed(token), vec![Action::Withdraw { token }]);
        // Only with the next focus.
        bridge.focus(false);
        set_parent(&bridge.focus(true));
    }

    #[test]
    fn a_cancel_while_pending_lets_the_next_selection_in() {
        let mut bridge = focused();
        let token = set_parent(&bridge.inner_changed(Some(types(&["a"]))));
        // The parent took ours, then someone else's, all before our sync.
        bridge.parent_changed(Some(types(&["a"])));
        bridge.cancelled(token);
        assert_eq!(
            bridge.parent_changed(Some(types(&["b"]))),
            vec![Action::SetInner {
                generation: 1,
                mime_types: types(&["b"])
            }]
        );
        assert!(bridge.confirmed(token).is_empty());
        assert_eq!(bridge.generation(), Some(1));
    }

    #[test]
    fn a_stale_cancel_is_ignored() {
        let mut bridge = focused();
        let first = set_parent(&bridge.inner_changed(Some(types(&["a"]))));
        bridge.parent_changed(Some(types(&["a"])));
        bridge.confirmed(first);
        let second = set_parent(&bridge.inner_changed(Some(types(&["b"]))));
        // The parent cancels the first when it takes the second.
        bridge.cancelled(first);
        assert!(bridge.is_current(second));
        bridge.parent_changed(Some(types(&["b"])));
        bridge.confirmed(second);
        assert!(bridge.parent_changed(Some(types(&["b"]))).is_empty());
    }

    #[test]
    fn an_owner_inside_that_goes_withdraws_the_source() {
        let mut bridge = focused();
        let token = set_parent(&bridge.inner_changed(Some(types(&["a"]))));
        bridge.parent_changed(Some(types(&["a"])));
        bridge.confirmed(token);
        assert_eq!(bridge.inner_changed(None), vec![Action::Withdraw { token }]);
        // The parent then has none (it says so, or someone else's).
        assert_eq!(bridge.parent_changed(None), vec![Action::ClearInner]);
    }

    #[test]
    fn a_copy_inside_without_the_focus_withdraws_the_stale_source() {
        let mut bridge = focused();
        let token = set_parent(&bridge.inner_changed(Some(types(&["a"]))));
        bridge.parent_changed(Some(types(&["a"])));
        bridge.confirmed(token);
        bridge.focus(false);
        assert_eq!(
            bridge.inner_changed(Some(types(&["b"]))),
            vec![Action::Withdraw { token }]
        );
        assert_eq!(
            bridge.focus(true),
            vec![Action::SetParent {
                token: token + 1,
                mime_types: types(&["b"])
            }]
        );
    }

    #[test]
    fn each_announcement_of_the_parent_is_a_new_generation() {
        let mut bridge = focused();
        for generation in 1..=3 {
            assert_eq!(
                bridge.parent_changed(Some(types(&["text/plain"]))),
                vec![Action::SetInner {
                    generation,
                    mime_types: types(&["text/plain"])
                }]
            );
        }
        // A copy inside replaces it inside.
        bridge.inner_changed(Some(types(&["x"])));
        assert_eq!(bridge.generation(), None);
    }

    #[test]
    fn only_to_the_parent() {
        let mut bridge = SelectionBridge::new(true, false);
        bridge.focus(true);
        assert!(bridge.parent_changed(Some(types(&["a"]))).is_empty());
        let token = set_parent(&bridge.inner_changed(Some(types(&["b"]))));
        // The echo still tells that the parent took it.
        assert!(bridge.parent_changed(Some(types(&["b"]))).is_empty());
        assert!(bridge.confirmed(token).is_empty());
        assert!(bridge.is_current(token));
    }

    #[test]
    fn only_from_the_parent() {
        let mut bridge = SelectionBridge::new(false, true);
        bridge.focus(true);
        assert!(bridge.inner_changed(Some(types(&["b"]))).is_empty());
        assert_eq!(
            bridge.parent_changed(Some(types(&["a"]))),
            vec![Action::SetInner {
                generation: 1,
                mime_types: types(&["a"])
            }]
        );
        // A copy inside stays inside; the parent's selection sent again
        // with the focus does not replace it.
        assert!(bridge.inner_changed(Some(types(&["c"]))).is_empty());
        assert!(bridge.parent_changed(Some(types(&["a"]))).is_empty());
        // A new one of the parent does.
        assert_eq!(
            bridge.parent_changed(Some(types(&["d"]))),
            vec![Action::SetInner {
                generation: 2,
                mime_types: types(&["d"])
            }]
        );
    }

    #[test]
    fn off() {
        let mut bridge = SelectionBridge::new(false, false);
        bridge.focus(true);
        assert!(bridge.inner_changed(Some(types(&["a"]))).is_empty());
        assert!(bridge.parent_changed(Some(types(&["b"]))).is_empty());
    }
}
