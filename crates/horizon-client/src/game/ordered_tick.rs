//! PURPOSE: ticks experience orbs, primed TNT, falling blocks, firework rockets and projectiles in the order
//! they joined the level, between the other kinds' batches.
//! WHY: `ServerLevel.tick` ticks every entity in join order, one kind among
//! the rest; these three used to tick as three fixed batches (orbs after the
//! mobs, falling blocks and TNT after the gateways), so a TNT that joined
//! before a boat blew up after the boat moved (#6509, tick order slice 2).
//! REFS: vanilla `ServerLevel.tick` (`entityTickList.forEach`),
//! `EntityTickList`; `join_seq.rs`; `tools/test_tick_order_parity.py`.
//! LOGS: none. READS-BY: `Game::tick_world`, `tick_mobs`.
//! ENDS-WHEN: every kind ticks from the join-order walk.

use super::{Game, Result};
use horizon_client::join_seq::{JoinSeq, Joined};
use horizon_core::random::RandomSource as _;

/// What this tick has ticked of the three kinds.
#[derive(Default, Clone)]
pub(super) struct OrderedTicks {
    /// Joins at or above this started after the tick began; they tick next tick.
    born: JoinSeq,
    /// The last join number ticked, if any: later ones are still to come.
    pub(super) done: Option<JoinSeq>,
    /// Joins below this have had their spawn packet; the ones above joined
    /// since (a summoned falling block, a lit TNT) and tick after it goes out.
    pub(super) announced: JoinSeq,
    /// For each batch of another kind, in tick order (see [`Batch`]): the
    /// lowest join number among it and every batch after it.
    suffix_min: [JoinSeq; Batch::COUNT],
    /// The orbs' lava random, made on first use.
    lava_random: Option<horizon_core::random::LegacyRandom>,
    /// The breeze deflection's jitter, drawn on the tick's first projectile batch.
    pub(super) jitter: Option<f32>,
    /// Set when an entity ticked here may have changed a mob, a boat or a
    /// block (anything but a projectile pass that hit nothing); the mob loop
    /// reads and clears it to know whether its view of the level is stale.
    pub(super) touched: bool,
}

/// The batches of the other kinds, in the order they tick.
#[derive(Clone, Copy)]
pub(super) enum Batch {
    ArmorStands,
    Boats,
    Carts,
    Tridents,
    Items,
    Mobs,
}

impl Batch {
    const COUNT: usize = 6;
}

#[derive(Clone, Copy)]
enum Pending {
    Orb,
    Tnt,
    Falling,
    Rocket,
    /// The projectile batch (wind charges, arrows, tridents' siblings, snowballs...).
    Projectile,
}

/// The lowest join number in a batch, or none when it is empty.
pub(super) fn first_join<T: Joined>(list: &[T]) -> Option<JoinSeq> {
    list.iter().map(Joined::join_seq).min()
}

/// Where the entity that joined as `join` is in `list`, if still there. A list
/// is usually in join order (spawns append), so a binary search finds it; a
/// list put back out of order falls back to the walk.
fn find_join<T: Joined>(list: &[T], join: JoinSeq) -> Option<usize> {
    match list.binary_search_by_key(&join, Joined::join_seq) {
        // Join numbers are unique, so a hit is the entity even in a list out of order.
        Ok(i) => Some(i),
        Err(_) => list.iter().position(|e| e.join_seq() == join),
    }
}

impl Game<'_> {
    /// A level tick starts: what has joined so far ticks this tick.
    pub(super) fn begin_ordered_ticks(&mut self) {
        let firsts = [
            first_join(&self.armor_stands),
            first_join(&self.boats),
            first_join(&self.carts),
            first_join(&self.tridents),
            first_join(&self.dropped),
            first_join(&self.mobs),
        ];
        // A batch ticks whole, so an entity of the three kinds goes after the
        // last batch that holds anything that joined before it.
        let mut suffix_min = [JoinSeq::MAX; Batch::COUNT];
        let mut lowest = JoinSeq::MAX;
        for (slot, first) in suffix_min.iter_mut().zip(firsts).rev() {
            lowest = lowest.min(first.unwrap_or(JoinSeq::MAX));
            *slot = lowest;
        }
        self.ordered = OrderedTicks {
            announced: self.ordered.announced,
            born: JoinSeq::upcoming(),
            done: None,
            suffix_min,
            lava_random: None,
            jitter: None,
            touched: false,
        };
    }

    /// The join number of the first orb, primed TNT, falling block, rocket or
    /// projectile still to tick (the mob loop's cue to stop and run it).
    pub(super) fn next_ordered_join(&self) -> Option<JoinSeq> {
        self.next_ordered_join_any()
    }

    fn next_ordered_join_any(&self) -> Option<JoinSeq> {
        let (done, born, announced) =
            (self.ordered.done, self.ordered.born, self.ordered.announced);
        let wanted = |j: JoinSeq| done.is_none_or(|d| j > d) && j < born;
        let orbs = self.orbs.iter().map(|e| e.join);
        let rockets = self.rockets.iter().map(|e| e.join);
        // A projectile stuck in a block ticks only its own age and fall check,
        // which no other entity can see: it never cuts the mob loop (it ticks
        // with the next pass, or the rest of the batch).
        let shots = self
            .creatures
            .projectiles
            .iter()
            .filter(|p| !p.stuck)
            .map(|p| p.join);
        let rest = self
            .tnts
            .iter()
            .map(|e| e.join)
            .chain(self.falling.iter().map(|e| e.join))
            .filter(|j| *j < announced);
        orbs.chain(rockets)
            .chain(shots)
            .chain(rest)
            .filter(|j| wanted(*j))
            .min()
    }

    /// Ticks, one at a time in join order, every orb, primed TNT and falling
    /// block that joined before `cut` and has not ticked this tick.
    pub(super) fn run_ordered_ticks(&mut self, cut: JoinSeq, late: bool) -> Result<()> {
        let mut first_error = None;
        let mut followed = None;
        let mut events = false;
        let entity_ticking = self.entity_ticking_area();
        // Who ticks in this run, sorted once: entities that join meanwhile are
        // at or above `born`, so the set cannot grow, only lose the removed.
        let (done, born, announced) =
            (self.ordered.done, self.ordered.born, self.ordered.announced);
        let wanted = |j: JoinSeq| done.is_none_or(|d| j > d) && j < cut && j < born;
        let mut pending: Vec<(JoinSeq, Pending)> = Vec::new();
        let mut offer = |j: JoinSeq, kind: Pending| {
            // Orbs are announced after their tick, always; TNT and falling
            // blocks that joined since the last packets wait for them.
            let right_pass = match kind {
                Pending::Orb | Pending::Rocket | Pending::Projectile => !late,
                _ => late == (j >= announced),
            };
            if right_pass && wanted(j) {
                pending.push((j, kind));
            }
        };
        self.orbs.iter().for_each(|e| offer(e.join, Pending::Orb));
        self.tnts.iter().for_each(|e| offer(e.join, Pending::Tnt));
        self.falling
            .iter()
            .for_each(|e| offer(e.join, Pending::Falling));
        self.rockets
            .iter()
            .for_each(|e| offer(e.join, Pending::Rocket));
        self.creatures
            .projectiles
            .iter()
            .for_each(|e| offer(e.join, Pending::Projectile));
        pending.sort_unstable_by_key(|&(j, _)| j);
        // The wind charges among them, found once (not a scan per projectile).
        let winds: std::collections::HashSet<JoinSeq> = self
            .creatures
            .projectiles
            .iter()
            .filter(|p| {
                matches!(
                    p.kind,
                    horizon_client::projectiles::ProjectileKind::WindCharge
                        | horizon_client::projectiles::ProjectileKind::BreezeWindCharge
                )
            })
            .map(|p| p.join)
            .collect();
        let wind = |_: &Self, join: JoinSeq| winds.contains(&join);
        // The rockets' targets, shared by consecutive rockets.
        let mut rocket_targets = None;
        let mut at = 0;
        while at < pending.len() {
            let (join, kind) = pending[at];
            at += 1;
            if matches!(kind, Pending::Projectile) {
                // Every projectile up to the next entity of another kind, in
                // one pass (the batch's setup is the cost, not the tick). A
                // wind charge ends the pass: its burst pushes what ticks after
                // it, so it goes out before the next projectile ticks.
                let mut last = join;
                let mut ended = wind(self, join);
                while !ended && let Some(&(j, Pending::Projectile)) = pending.get(at) {
                    last = j;
                    at += 1;
                    ended = wind(self, j);
                }
                let hi = if ended {
                    last.after()
                } else {
                    pending.get(at).map_or(cut.min(born), |&(j, _)| j)
                };
                self.ordered.done = Some(last);
                rocket_targets = None;
                if let Err(e) = self.tick_projectiles(join, hi) {
                    first_error.get_or_insert(e);
                }
                continue;
            }
            self.ordered.done = Some(join);
            self.ordered.touched = true;
            if !matches!(kind, Pending::Rocket) {
                rocket_targets = None;
            }
            // ServerLevel.tick: only entities in entity-ticking chunks tick.
            match kind {
                Pending::Tnt => {
                    let Some(i) = find_join(&self.tnts, join) else {
                        continue;
                    };
                    let at = self.tnts[i].position;
                    if entity_ticking(at.x.floor() as i32, at.z.floor() as i32)
                        && let Err(e) = self.tick_one_tnt(i)
                    {
                        first_error.get_or_insert(e);
                    }
                }
                Pending::Falling => {
                    let Some(i) = find_join(&self.falling, join) else {
                        continue;
                    };
                    let at = self.falling[i].position;
                    if entity_ticking(at.x.floor() as i32, at.z.floor() as i32) {
                        events = true;
                        if let Err(e) = self.tick_one_falling(i) {
                            first_error.get_or_insert(e);
                        }
                    }
                }
                Pending::Projectile => {}
                Pending::Rocket => {
                    if let Some(i) = self.rockets.iter().position(|e| e.join == join) {
                        let targets = rocket_targets.get_or_insert_with(|| self.rocket_targets());
                        let before = self.rockets.len();
                        self.tick_one_rocket(i, targets);
                        if self.rockets.len() != before {
                            rocket_targets = None;
                        }
                    }
                }
                Pending::Orb => {
                    let Some(i) = find_join(&self.orbs, join) else {
                        continue;
                    };
                    let followed = followed.get_or_insert_with(|| self.orb_followed());
                    let o = &mut self.orbs[i];
                    let distance = |feet: &[f64; 3]| -> f64 {
                        (feet[0] - o.position.x).powi(2)
                            + (feet[1] - o.position.y).powi(2)
                            + (feet[2] - o.position.z).powi(2)
                    };
                    let target = followed
                        .iter()
                        .filter(|(feet, _)| distance(feet) < 64.0)
                        .min_by(|a, b| distance(&a.0).total_cmp(&distance(&b.0)))
                        .map(|(_, target)| *target);
                    // An orb has no random of its own here: lava draws from one
                    // made on first use, apart from the level stream.
                    let random = &mut self.ordered.lava_random;
                    let mut next_float = || {
                        random
                            .get_or_insert_with(super::dispenser::entity_random)
                            .next_float()
                    };
                    let stays = o.tick(
                        &self.world,
                        self.res.facts(),
                        self.collision,
                        target,
                        self.remote.is_none(),
                        &mut next_float,
                        Some(self.border.geometry()),
                        &self.res.motion().fluids,
                    );
                    if !stays {
                        self.orbs.remove(i);
                    }
                }
            }
        }
        if events && let Err(e) = self.process_level_events() {
            first_error.get_or_insert(e);
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Before a batch of another kind ticks: the three kinds' entities that
    /// joined before everything still to tick in the batches from this one on.
    pub(super) fn ordered_before(&mut self, batch: Batch) -> Result<()> {
        let cut = self.ordered.suffix_min[batch as usize];
        if cut == JoinSeq::MAX {
            return Ok(());
        }
        self.run_ordered_ticks(cut, false)
    }

    /// The projectile batch, whole: whatever of it has not ticked, after the
    /// orbs, TNT, falling blocks and rockets that joined before it.
    pub(super) fn tick_projectile_rest(&mut self) -> Result<()> {
        let born = self.ordered.born;
        let last = self
            .creatures
            .projectiles
            .iter()
            .map(|p| p.join)
            .filter(|j| *j < born)
            .max();
        if let Some(last) = last {
            self.run_ordered_ticks(last.after(), false)?;
        }
        // Shot during this tick: they tick with the batch, as before.
        self.tick_projectiles(born, JoinSeq::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::find_join;
    use horizon_client::join_seq::{JoinList, JoinSeq, Joined as _};

    #[derive(Clone)]
    struct E {
        join: JoinSeq,
    }
    horizon_client::impl_joined!(E);

    /// The run looks each entity up by join number: in a list in join order,
    /// one put back out of order, and after the entity left.
    #[test]
    fn find_join_finds_by_number_in_or_out_of_order() {
        let mut list = JoinList::default();
        for _ in 0..5 {
            list.push(E {
                join: JoinSeq::UNJOINED,
            });
        }
        let joins: Vec<JoinSeq> = list.iter().map(|e| e.join_seq()).collect();
        for (i, &j) in joins.iter().enumerate() {
            assert_eq!(find_join(&list, j), Some(i));
        }
        list.swap(0, 4);
        list.swap(1, 3);
        assert_eq!(find_join(&list, joins[0]), Some(4));
        assert_eq!(find_join(&list, joins[4]), Some(0));
        assert_eq!(find_join(&list, joins[1]), Some(3));
        let gone = list.remove(2).join_seq();
        assert_eq!(find_join(&list, gone), None);
    }
}
