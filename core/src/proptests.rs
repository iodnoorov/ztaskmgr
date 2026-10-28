//! Свойства формата и слияния на случайных страницах: то, что примеры в тестах модулей
//! проверяют на нескольких случаях, здесь — на сотнях.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use crate::clock::Date;
use crate::lists::Lists;
use crate::merge::merge_boards;
use crate::model::{Board, ListId, State, Task};
use crate::storage::{parse, serialize};

fn list_id() -> impl Strategy<Value = ListId> {
    prop_oneof![
        Just(ListId::WORK),
        Just(ListId::HOME),
        Just(ListId::PERSONAL),
        Just(ListId::OTHER),
        Just(ListId::new("zz9")),
    ]
}

fn state() -> impl Strategy<Value = State> {
    prop_oneof![
        Just(State::Open),
        Just(State::Done),
        Just(State::Cancelled),
        (1u32..28).prop_map(|day| State::Migrated(Date::new(2026, 11, day).unwrap())),
    ]
}

/// Текст задачи: без служебных последовательностей, переводов строк и пробелов по краям
/// (их срезает разбор строки).
fn text() -> impl Strategy<Value = String> {
    "[a-zA-Zа-яё0-9.,!?()\\[\\] -]{0,30}".prop_map(|t| t.trim().to_string())
}

fn task(id: Option<String>) -> impl Strategy<Value = Task> {
    (text(), state(), any::<bool>(), 0u64..5).prop_map(move |(text, state, private, modified)| {
        let mut task = Task::new(text);
        task.state = state;
        task.private = private;
        task.modified = modified * 1_000;
        task.id = id.clone();
        task
    })
}

/// Страница: задачи с уникальными id из небольшого набора (чтобы версии пересекались)
/// и надгробия.
fn board() -> impl Strategy<Value = Board> {
    let ids = proptest::sample::subsequence((0..12).map(|n| format!("t{n}")).collect::<Vec<_>>(), 0..8);
    (ids, proptest::collection::vec(list_id(), 8), proptest::collection::vec(0u64..5, 0..3))
        .prop_flat_map(|(ids, lists, graves)| {
            let tasks: Vec<_> = ids.iter().map(|id| task(Some(id.clone()))).collect();
            (Just(ids), Just(lists), Just(graves), tasks)
        })
        .prop_map(|(ids, lists, graves, tasks)| {
            let mut board = Board::default();
            for (i, task) in tasks.into_iter().enumerate() {
                board.push(lists[i % lists.len()], task);
            }
            for (n, when) in graves.into_iter().enumerate() {
                // Надгробия — по id, которых на странице нет.
                let id = format!("g{n}");
                if !ids.contains(&id) {
                    board.bury(&id, when * 1_000 + 500);
                }
            }
            board
        })
}

/// Задачи страницы по id: (список, текст, состояние).
fn by_id(board: &Board) -> BTreeMap<String, (ListId, String, State)> {
    let mut out = BTreeMap::new();
    for list in board.ids() {
        for t in board.list(list) {
            out.insert(t.id.clone().unwrap(), (list, t.text.clone(), t.state));
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

    /// Записанная страница читается обратно той же — задачи, их порядок, надгробия.
    #[test]
    fn roundtrip(board in board()) {
        let lists = Lists::default();
        let parsed = parse(&serialize(&board, &lists), &lists).unwrap();
        prop_assert_eq!(&parsed, &board);
        prop_assert_eq!(parsed.graves(), board.graves());
        for list in board.ids() {
            prop_assert_eq!(parsed.list(list), board.list(list));
        }
    }

    /// Слияние страницы с самой собой ничего не меняет.
    #[test]
    fn merge_idempotent(board in board()) {
        prop_assert_eq!(merge_boards(&board, &board), board);
    }

    /// Без надгробий ни одна задача не теряется.
    #[test]
    fn merge_keeps_everything(a in board(), b in board()) {
        let (mut a, mut b) = (a, b);
        for board in [&mut a, &mut b] {
            let graves: Vec<String> = board.graves().iter().map(|(id, _)| id.clone()).collect();
            let mut clean = Board::default();
            for list in board.ids() {
                for t in board.list(list) {
                    clean.push(list, t.clone());
                }
            }
            prop_assert!(graves.iter().all(|g| clean.buried(g).is_none()));
            *board = clean;
        }
        let merged = by_id(&merge_boards(&a, &b));
        let ids: BTreeSet<String> = by_id(&a).into_keys().chain(by_id(&b).into_keys()).collect();
        prop_assert_eq!(merged.into_keys().collect::<BTreeSet<_>>(), ids);
    }

    /// Задачи, созданные программой (у них есть время правки), и с разным временем у общих
    /// задач: итог не зависит от того, какая версия «наша» — те же задачи с тем же текстом,
    /// состоянием и списком.
    #[test]
    fn merge_commutes_with_distinct_times(a in board(), b in board()) {
        let (mut a, mut b) = (a, b);
        for list in a.ids() {
            for i in 0..a.list(list).len() {
                a.get_mut(list, i).unwrap().modified += 2;
            }
        }
        let mine: BTreeMap<_, _> = a.tasks().map(|t| (t.id.clone().unwrap(), t.modified)).collect();
        for list in b.ids() {
            for i in 0..b.list(list).len() {
                let task = b.get_mut(list, i).unwrap();
                task.modified += 2;
                if mine.get(task.id.as_ref().unwrap()) == Some(&task.modified) {
                    task.modified += 1;
                }
            }
        }
        prop_assert_eq!(by_id(&merge_boards(&a, &b)), by_id(&merge_boards(&b, &a)));
    }
}
