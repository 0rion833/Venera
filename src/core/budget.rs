//! Context budget manager (`TOOL.md` §9).
//!
//! One place decides how many tokens a request may spend on each part of the context, so
//! the map, the retrieved source, and the history cannot each optimistically assume they
//! own the whole window.
//!
//! The formula is the one from the specification:
//!
//! ```text
//! usable = window - system - tools - instructions - history - user
//!          - reserved_output - safety_margin
//! ```
//!
//! Everything is saturating: a window smaller than the fixed parts yields zero usable
//! tokens, never a negative number cast to `usize`.

/// Share of the usable budget the project map may take.
pub const MAP_SHARE: f64 = 0.05;
/// Share of the usable budget retrieved source may take.
pub const RETRIEVAL_SHARE: f64 = 0.45;
/// Share of the usable budget the conversation may take.
pub const CONVERSATION_SHARE: f64 = 0.20;
/// Smallest useful map, so a tiny window still produces something.
pub const MIN_MAP_TOKENS: u64 = 200;
/// Smallest reply cap that still leaves room for a tool call.
pub const MIN_OUTPUT_TURN: u64 = 128;

/// Resolves the ceiling on one turn's reply.
///
/// Precedence: an explicit `configured` value wins, otherwise the operation's `suggested`
/// ceiling; the model's own `model_max` is a hard upper bound, and the result never drops
/// below [`MIN_OUTPUT_TURN`] (or the model maximum, when that is even lower).
pub fn turn_output_cap(configured: Option<u64>, suggested: u64, model_max: Option<u64>) -> u64 {
    let ceiling = model_max.unwrap_or(u64::MAX);
    let floor = MIN_OUTPUT_TURN.min(ceiling);
    configured.unwrap_or(suggested).min(ceiling).max(floor)
}

/// Tokens already committed before project context is considered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Committed {
    /// System prompt.
    pub system: u64,
    /// Tool schemas.
    pub tools: u64,
    /// AGENTS.md digest.
    pub instructions: u64,
    /// Previous turns.
    pub history: u64,
    /// The current user message.
    pub user: u64,
}

impl Committed {
    /// Sum of every fixed part.
    pub fn total(&self) -> u64 {
        self.system + self.tools + self.instructions + self.history + self.user
    }
}

/// How the usable part is split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    /// Tokens available for project context at all.
    pub usable: u64,
    /// Project map allowance.
    pub project_map: u64,
    /// Retrieved source allowance.
    pub retrieval: u64,
    /// Conversation allowance on top of the history already committed.
    pub conversation: u64,
    /// Left unassigned, so one oversized category cannot starve the others.
    pub free: u64,
}

/// Window size and reserves of one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// Model context window.
    pub window: u64,
    /// Space kept for the reply.
    pub reserve_output: u64,
    /// Hard safety stop below the window.
    pub safety_margin: u64,
}

impl ContextBudget {
    /// Builds a budget for a model with an explicit output reserve.
    pub fn new(window: u64, reserve_output: u64) -> Self {
        Self {
            window,
            reserve_output,
            safety_margin: (window as f64 * 0.05) as u64,
        }
    }

    /// Uses the model's `max_output` as the reserve, capped at a quarter of the window.
    pub fn for_model(window: u64, max_output: Option<u64>) -> Self {
        let default_output = (window / 10).max(1_024);
        let reserve = max_output.unwrap_or(default_output).min(window / 4);
        Self::new(window, reserve)
    }

    /// Budget for one turn whose reply is capped by `limits.max_output_tokens_per_turn`.
    ///
    /// The reserve is the cap itself, not the model's maximum: the point of the cap is to
    /// keep the reply small, and a reserve of 131k reserves nothing at all.
    pub fn for_turn(window: u64, model_max: Option<u64>, suggested: u64) -> Self {
        let cap = turn_output_cap(None, suggested, model_max);
        Self::new(window, cap.min((window / 4).max(1)))
    }

    /// Tokens left for project context after everything fixed.
    pub fn usable(&self, committed: &Committed) -> u64 {
        self.window
            .saturating_sub(committed.total())
            .saturating_sub(self.reserve_output)
            .saturating_sub(self.safety_margin)
    }

    /// Splits the usable part between map, retrieval, and conversation.
    pub fn allocate(&self, committed: &Committed) -> Allocation {
        let usable = self.usable(committed);
        let share = |ratio: f64| (usable as f64 * ratio) as u64;

        let project_map = share(MAP_SHARE).max(MIN_MAP_TOKENS.min(usable));
        let retrieval = share(RETRIEVAL_SHARE);
        let conversation = share(CONVERSATION_SHARE);

        // The floors must not push the allocation past the usable budget.
        let assigned = project_map
            .saturating_add(retrieval)
            .saturating_add(conversation)
            .min(usable);

        Allocation {
            usable,
            project_map,
            retrieval,
            conversation,
            free: usable - assigned,
        }
    }

    /// Whether a request of `tokens` fits at all.
    pub fn fits(&self, tokens: u64) -> bool {
        tokens
            .saturating_add(self.reserve_output)
            .saturating_add(self.safety_margin)
            <= self.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luna() -> ContextBudget {
        ContextBudget::for_model(1_050_000, Some(131_072))
    }

    #[test]
    fn subtracts_every_fixed_part() {
        let budget = ContextBudget::new(10_000, 2_000);
        let committed = Committed {
            system: 120,
            tools: 900,
            instructions: 80,
            history: 3_000,
            user: 40,
        };

        assert_eq!(committed.total(), 4_140);
        assert_eq!(budget.usable(&committed), 10_000 - 4_140 - 2_000 - 500);
    }

    #[test]
    fn a_tiny_window_yields_zero_instead_of_underflow() {
        let budget = ContextBudget::new(1_000, 900);
        let committed = Committed {
            system: 200,
            ..Committed::default()
        };

        assert_eq!(budget.usable(&committed), 0);
        assert_eq!(budget.allocate(&committed).usable, 0);
    }

    #[test]
    fn allocations_stay_inside_the_usable_budget() {
        let budget = luna();
        let committed = Committed {
            system: 400,
            tools: 1_200,
            instructions: 120,
            history: 20_000,
            user: 60,
        };
        let allocation = budget.allocate(&committed);

        assert!(allocation.usable > 0);
        assert!(
            allocation.project_map + allocation.retrieval + allocation.conversation
                <= allocation.usable
        );
        assert_eq!(
            allocation.free,
            allocation.usable
                - allocation.project_map
                - allocation.retrieval
                - allocation.conversation
        );
    }

    #[test]
    fn the_map_always_gets_at_least_the_minimum() {
        let budget = ContextBudget::new(6_000, 1_000);
        let allocation = budget.allocate(&Committed::default());

        assert!(
            allocation.project_map >= MIN_MAP_TOKENS,
            "got {}",
            allocation.project_map
        );
        assert!(allocation.retrieval > allocation.project_map);
    }

    #[test]
    fn the_output_reserve_is_capped_at_a_quarter_of_the_window() {
        let budget = ContextBudget::for_model(8_000, Some(100_000));

        assert_eq!(budget.reserve_output, 2_000);
        assert_eq!(budget.safety_margin, 400);
    }

    #[test]
    fn a_request_fits_only_with_its_reserves() {
        let budget = ContextBudget::new(10_000, 1_000);

        assert!(budget.fits(8_500));
        assert!(!budget.fits(8_600));
    }

    #[test]
    fn the_operation_ceiling_wins_over_the_model_maximum() {
        // Luna: max_output 131072, edit turns suggest 1200.
        assert_eq!(turn_output_cap(None, 1_200, Some(131_072)), 1_200);
        // An explicit config value beats the suggestion.
        assert_eq!(turn_output_cap(Some(400), 1_200, Some(131_072)), 400);
        // The model maximum is a hard bound even for a configured value.
        assert_eq!(turn_output_cap(Some(9_000), 1_200, Some(4_096)), 4_096);
        // No model maximum, no config: the suggestion stands.
        assert_eq!(turn_output_cap(None, 600, None), 600);
    }

    #[test]
    fn the_turn_cap_never_falls_below_the_minimum() {
        assert_eq!(
            turn_output_cap(Some(10), 1_200, Some(131_072)),
            MIN_OUTPUT_TURN
        );
        // A model whose whole maximum is tiny cannot be pushed above it.
        assert_eq!(turn_output_cap(Some(10), 1_200, Some(64)), 64);
    }

    #[test]
    fn a_turn_reserves_its_cap_not_the_model_maximum() {
        let budget = ContextBudget::for_turn(1_050_000, Some(131_072), 2_000);

        assert_eq!(budget.reserve_output, 2_000);
        assert!(budget.usable(&Committed::default()) > 990_000);
    }

    #[test]
    fn a_turn_cap_stays_inside_a_quarter_of_the_window() {
        let budget = ContextBudget::for_turn(8_000, Some(131_072), 4_000);

        assert_eq!(budget.reserve_output, 2_000);
    }
}
