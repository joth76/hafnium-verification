/*
 * Copyright 2018 Google LLC
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

#include <assert.h>
#include <stdalign.h>
#include <stdint.h>

#include "hf/mm.h"
#include "hf/std.h"

#include "vmapi/hf/call.h"

#include "hftest.h"

static alignas(PAGE_SIZE) uint8_t send_page[PAGE_SIZE];
static alignas(PAGE_SIZE) uint8_t recv_page[PAGE_SIZE];
static_assert(sizeof(send_page) == PAGE_SIZE, "Send page is not a page.");
static_assert(sizeof(recv_page) == PAGE_SIZE, "Recv page is not a page.");

static hf_ipaddr_t send_page_addr = (hf_ipaddr_t)send_page;
static hf_ipaddr_t recv_page_addr = (hf_ipaddr_t)recv_page;

/* Keep macro alignment */
/* clang-format off */

#define RELAY_A_VM_ID 1
#define RELAY_B_VM_ID 2
#define ECHO_VM_ID    3

/* clang-format on */

/**
 * Confirm there are 3 secondary VMs as well as this primary VM.
 */
TEST(hf_vm_get_count, three_secondary_vms)
{
	EXPECT_EQ(hf_vm_get_count(), 4);
}

/**
 * Confirm there that secondary VM has 1 VCPU.
 */
TEST(hf_vcpu_get_count, secondary_has_one_vcpu)
{
	EXPECT_EQ(hf_vcpu_get_count(1), 1);
}

/**
 * Confirm it is an error to query how many VCPUs are assigned to a nonexistent
 * secondary VM.
 */
TEST(hf_vcpu_get_count, large_invalid_vm_index)
{
	EXPECT_EQ(hf_vcpu_get_count(0xffffffff), -1);
}

/**
 * The primary can't be run by the hypervisor.
 */
TEST(hf_vcpu_run, cannot_run_primary)
{
	struct hf_vcpu_run_return res = hf_vcpu_run(HF_PRIMARY_VM_ID, 0);
	EXPECT_EQ(res.code, HF_VCPU_RUN_WAIT_FOR_INTERRUPT);
}

/**
 * Can only run a VM that exists.
 */
TEST(hf_vcpu_run, cannot_run_absent_secondary)
{
	struct hf_vcpu_run_return res = hf_vcpu_run(1234, 0);
	EXPECT_EQ(res.code, HF_VCPU_RUN_WAIT_FOR_INTERRUPT);
}

/**
 * Can only run a vcpu that exists.
 */
TEST(hf_vcpu_run, cannot_run_absent_vcpu)
{
	struct hf_vcpu_run_return res = hf_vcpu_run(ECHO_VM_ID, 1234);
	EXPECT_EQ(res.code, HF_VCPU_RUN_WAIT_FOR_INTERRUPT);
}

/**
 * The configured send/receive addresses can't be unaligned.
 */
TEST(hf_vm_configure, fails_with_unaligned_pointer)
{
	uint8_t maybe_aligned[2];
	hf_ipaddr_t unaligned_addr = (hf_ipaddr_t)&maybe_aligned[1];
	hf_ipaddr_t aligned_addr = (hf_ipaddr_t)send_page;

	/* Check the the address is unaligned. */
	ASSERT_EQ(unaligned_addr & 1, 1);

	EXPECT_EQ(hf_vm_configure(aligned_addr, unaligned_addr), -1);
	EXPECT_EQ(hf_vm_configure(unaligned_addr, aligned_addr), -1);
	EXPECT_EQ(hf_vm_configure(unaligned_addr, unaligned_addr), -1);
}

/**
 * The configured send/receive addresses can't be the same page.
 */
TEST(hf_vm_configure, fails_with_same_page)
{
	EXPECT_EQ(hf_vm_configure(send_page_addr, send_page_addr), -1);
	EXPECT_EQ(hf_vm_configure(recv_page_addr, recv_page_addr), -1);
}

/**
 * The configuration of the send/receive addresses can only happen once.
 */
TEST(hf_vm_configure, fails_if_already_succeeded)
{
	EXPECT_EQ(hf_vm_configure(send_page_addr, recv_page_addr), 0);
	EXPECT_EQ(hf_vm_configure(send_page_addr, recv_page_addr), -1);
}

/**
 * The configuration of the send/receive address is successful with valid
 * arguments.
 */
TEST(hf_vm_configure, succeeds)
{
	EXPECT_EQ(hf_vm_configure(send_page_addr, recv_page_addr), 0);
}

/**
 * The primary receives messages from hf_vcpu_run().
 */
TEST(hf_mailbox_receive, cannot_receive_from_primary_blocking)
{
	struct hf_mailbox_receive_return res = hf_mailbox_receive(true);
	EXPECT_EQ(res.vm_id, HF_INVALID_VM_ID);
	EXPECT_EQ(res.size, 0);
}

/**
 * The primary receives messages from hf_vcpu_run().
 */
TEST(hf_mailbox_receive, cannot_receive_from_primary_non_blocking)
{
	struct hf_mailbox_receive_return res = hf_mailbox_receive(false);
	EXPECT_EQ(res.vm_id, HF_INVALID_VM_ID);
	EXPECT_EQ(res.size, 0);
}

/**
 * Send and receive the same message from the echo VM.
 */
TEST(mailbox, echo)
{
	const char message[] = "Echo this back to me!";
	struct hf_vcpu_run_return run_res;

	/* Configure mailbox pages. */
	EXPECT_EQ(hf_vm_configure(send_page_addr, recv_page_addr), 0);
	run_res = hf_vcpu_run(ECHO_VM_ID, 0);
	EXPECT_EQ(run_res.code, HF_VCPU_RUN_WAIT_FOR_INTERRUPT);

	/* Set the message, echo it and check it didn't change. */
	memcpy(send_page, message, sizeof(message));
	EXPECT_EQ(hf_mailbox_send(ECHO_VM_ID, sizeof(message)), 0);
	run_res = hf_vcpu_run(ECHO_VM_ID, 0);
	EXPECT_EQ(run_res.code, HF_VCPU_RUN_MESSAGE);
	EXPECT_EQ(run_res.message.size, sizeof(message));
	EXPECT_EQ(memcmp(recv_page, message, sizeof(message)), 0);
	EXPECT_EQ(hf_mailbox_clear(), 0);
}

/**
 * Send a message to relay_a which will forward it to relay_b where it will be
 * sent back here.
 */
TEST(mailbox, relay)
{
	const char message[] = "Send this round the relay!";
	struct hf_vcpu_run_return run_res;

	/* Configure mailbox pages. */
	EXPECT_EQ(hf_vm_configure(send_page_addr, recv_page_addr), 0);
	run_res = hf_vcpu_run(RELAY_A_VM_ID, 0);
	EXPECT_EQ(run_res.code, HF_VCPU_RUN_WAIT_FOR_INTERRUPT);
	run_res = hf_vcpu_run(RELAY_B_VM_ID, 0);
	EXPECT_EQ(run_res.code, HF_VCPU_RUN_WAIT_FOR_INTERRUPT);

	/*
	 * Send the message to relay_a which is then sent to relay_b before
	 * checking that relay_b send the message back here.
	 */
	memcpy(send_page, message, sizeof(message));
	EXPECT_EQ(hf_mailbox_send(RELAY_A_VM_ID, sizeof(message)), 0);
	run_res = hf_vcpu_run(RELAY_A_VM_ID, 0);
	EXPECT_EQ(run_res.code, HF_VCPU_RUN_WAKE_UP);
	EXPECT_EQ(run_res.wake_up.vm_id, RELAY_B_VM_ID);
	EXPECT_EQ(run_res.wake_up.vcpu, 0);
	run_res = hf_vcpu_run(RELAY_B_VM_ID, 0);
	EXPECT_EQ(run_res.code, HF_VCPU_RUN_MESSAGE);
	EXPECT_EQ(run_res.message.size, sizeof(message));
	EXPECT_EQ(memcmp(recv_page, message, sizeof(message)), 0);
	EXPECT_EQ(hf_mailbox_clear(), 0);
}