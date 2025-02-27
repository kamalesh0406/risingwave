// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(incomplete_features)]
#![expect(dead_code)]
#![allow(clippy::derive_partial_eq_without_eq)]
#![feature(trait_alias)]
#![feature(binary_heap_drain_sorted)]
#![feature(exact_size_is_empty)]
#![feature(type_alias_impl_trait)]
#![cfg_attr(coverage, feature(no_coverage))]
#![feature(generators)]
#![feature(proc_macro_hygiene, stmt_expr_attributes)]
#![feature(iterator_try_collect)]
#![feature(lint_reasons)]
#![feature(binary_heap_into_iter_sorted)]
#![feature(is_sorted)]
#![recursion_limit = "256"]
#![feature(let_chains)]
#![feature(bound_map)]
#![feature(int_roundings)]
#![feature(async_fn_in_trait)]

mod error;
pub mod exchange_source;
pub mod execution;
pub mod executor;
pub mod rpc;
pub mod task;

#[macro_use]
extern crate tracing;
#[macro_use]
extern crate risingwave_common;
