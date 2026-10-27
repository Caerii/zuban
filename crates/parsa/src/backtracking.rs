use crate::{InternalNode, automaton::InternalSquashedType, grammar::Token};

#[derive(Debug)]
pub struct BacktrackingTokenizer<T: Token, I: Iterator<Item = T>> {
    tokenizer: I,
    tokens: Vec<(T, InternalSquashedType)>,
    next_index: usize,
    is_recording: bool,
    last_transition: Option<InternalSquashedType>,
}

impl<T: Token, I: Iterator<Item = T>> BacktrackingTokenizer<T, I> {
    pub fn new(tokenizer: I) -> Self {
        Self {
            tokenizer,
            tokens: vec![],
            next_index: 0,
            is_recording: false,
            last_transition: None,
        }
    }

    #[inline]
    pub fn start(&mut self, token: &T) -> usize {
        self.is_recording = true;
        if self.tokens.is_empty() {
            self.tokens.push((
                *token,
                self.last_transition
                    .expect("started backtracking before classifying token"),
            ));
            self.next_index = 1;
            0
        } else {
            self.next_index - 1
        }
    }

    #[inline]
    pub fn cached_transition(&self) -> Option<InternalSquashedType> {
        self.last_transition
    }

    #[inline]
    pub fn record_transition(&mut self, transition: InternalSquashedType) {
        if self.last_transition.is_none() && self.is_recording && self.next_index > 0 {
            if let Some((_, recorded)) = self.tokens.get_mut(self.next_index - 1) {
                *recorded = transition;
            }
        }
        self.last_transition = Some(transition);
    }

    #[inline]
    pub fn reset(&mut self, token_index: usize) {
        self.next_index = token_index;
    }

    #[inline]
    pub fn stop(&mut self) {
        self.is_recording = false;
    }

    pub fn reset_to_last_leaf(&mut self, leaf: &InternalNode) {
        debug_assert!(self.is_recording);
        for (i, (token, _)) in self.tokens.iter().enumerate().rev() {
            if token.start_index() == leaf.start_index && token.length() == leaf.length {
                debug_assert_ne!(leaf.length, 0);
                self.next_index = i + 1;
                return;
            }
        }
        unreachable!("Expected a last leaf when backtracking");
    }
}

impl<T: Token, I: Iterator<Item = T>> Iterator for BacktrackingTokenizer<T, I> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        if !self.tokens.is_empty() {
            match self.tokens.get(self.next_index) {
                None => {
                    let next = self.tokenizer.next();
                    self.last_transition = None;
                    if self.is_recording {
                        self.next_index += 1;
                        if let Some(token) = next {
                            // Grammar fills this placeholder before the token can be replayed.
                            self.tokens.push((token, InternalSquashedType(u16::MAX)));
                        }
                    } else {
                        self.next_index = 0;
                        self.tokens.clear();
                    }
                    next
                }
                Some(next) => {
                    self.next_index += 1;
                    debug_assert_ne!(next.1, InternalSquashedType(u16::MAX));
                    self.last_transition = Some(next.1);
                    Some(next.0)
                }
            }
        } else {
            // is_recording is only true if there's at least one token.
            self.last_transition = None;
            self.tokenizer.next()
        }
    }
}
