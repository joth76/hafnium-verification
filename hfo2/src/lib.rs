/*
 * Copyright 2019 Jeehoon Kang
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#![no_std]
#![feature(const_fn)]
#![feature(const_panic)]
#![feature(ptr_wrapping_offset_from)]

#[macro_use]
extern crate bitflags;
#[macro_use]
extern crate static_assertions;
extern crate reduce;
extern crate arrayvec;

mod cpio;
#[macro_use]
mod utils;
#[macro_use]
mod dlog;
mod api;
mod cpu;
mod list;
mod memiter;
mod mm;
mod mpool;
mod page;
mod panic;
mod spinlock;
mod std;
mod types;
mod vm;
