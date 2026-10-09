#[test_only]
module agent_api_test_coin::wrapper_execution_test;

use agent_api_test_coin::accounting::{Self, AgentWallet, Cashier};
use agent_api_test_coin::authorize;
use agent_api_test_coin::charge;
use agent_api_test_coin::register;
use agent_api_test_coin::revoke;
use nexus_interface::agent as agent_interface;
use nexus_interface::authorization::AgentVertexAuthorization;
use nexus_interface::dag::{Self as dag, DAG};
use nexus_interface::graph;
use nexus_interface::meta_schema;
use nexus_interface::onchain_tool_result::{Self as result_api, OnchainToolResult};
use nexus_interface::payment as payment_interface;
use nexus_primitives::authorization::ProvenValue;
use nexus_primitives::data::{Self as data};
use nexus_primitives::owner_cap::CloneableOwnerCap;
use nexus_primitives::proof_of_uid::UIDRequirements;
use nexus_kernel::runtime_authority;
use nexus_registry::agent_registry;
use nexus_registry::{leader, leader_cap};
use nexus_registry::network_auth;
use nexus_scheduler::{scheduler, task};
use nexus_scheduler::task::Task;
use nexus_scheduler::era as scheduler_era;
use nexus_tool::tool_registry;
use nexus_workflow::{execution, execution_submission, invocation_adapter, test_utils};
use std::ascii::{Self, String as AsciiString};
use std::unit_test::destroy;
use sui::balance;
use sui::clock;
use sui::coin;
use sui::object::{Self, ID};
use sui::sui::SUI;
use sui::test_scenario;
use sui::vec_map;
use nexus_primitives::tagged_output;
use talus::us::US;

const OWNER: address = @0xA;
const TOOL_VERTEX: vector<u8> = b"agent_api_wrapper";

public struct AuthorizedCall {
    agent: agent_interface::Agent,
    wallet: AgentWallet<agent_api_test_coin::test_coin::TEST_COIN>,
    cashier: Cashier<agent_api_test_coin::test_coin::TEST_COIN>,
    operator_cap: accounting::OperatorCap<agent_api_test_coin::test_coin::TEST_COIN>,
    settlement_cap: accounting::SettlementCap<agent_api_test_coin::test_coin::TEST_COIN>,
    authorization: ProvenValue<AgentVertexAuthorization>,
    requirements: UIDRequirements,
    result: OnchainToolResult,
    result_id: ID,
    binding_id: ID,
    input_hash: vector<u8>,
}

fun binding_id(): ID {
    object::id_from_address(@0xB1)
}

fun data_port(name: vector<u8>): meta_schema::PortSchema {
    meta_schema::port_schema(name, false, meta_schema::value_kind_data())
}

fun object_port(name: vector<u8>): meta_schema::PortSchema {
    meta_schema::port_schema(name, false, meta_schema::value_kind_object())
}

fun schema(output: vector<u8>, output_ports: vector<meta_schema::PortSchema>): meta_schema::MetaSchema {
    meta_schema::new(
        vector[data_port(b"0"), data_port(b"1")],
        vector[meta_schema::output_variant_schema(output, output_ports)],
    )
}

fun ensure_admission_leader_selected(
    registry: &mut leader::LeaderRegistry,
    primary_cap: &CloneableOwnerCap<leader_cap::OverNetwork>,
    seed: &vector<u8>,
    clock: &clock::Clock,
    ctx: &mut sui::tx_context::TxContext,
) {
    let primary_id = object::id(primary_cap);
    let mut next_stake = 2;
    let mut ranking = leader::rank_active_leaders_stake_weighted<scheduler_era::WorkAdmissionV1>(
        registry,
        seed,
    );
    while (ranking[0] != primary_id || ranking[1] == primary_id) {
        let mut stake_coin = coin::mint_for_testing<US>(next_stake, ctx);
        leader::stake(registry, primary_id, &mut stake_coin, next_stake, clock, ctx);
        coin::destroy_zero(stake_coin);
        next_stake = next_stake * 2;
        ranking = leader::rank_active_leaders_stake_weighted<scheduler_era::WorkAdmissionV1>(
            registry,
            seed,
        );
    };
}

/// Builds a real Agent-issued, wallet-recipient-bound workflow result for one concrete Tool FQN.
fun new_authorized_call(
    scenario: &mut test_scenario::Scenario,
    operation: vector<u8>,
): AuthorizedCall {
    let (
        clock,
        agent_registry,
        agent,
        wallet,
        cashier,
        operator_cap,
        settlement_cap,
        tool_registry,
        registry_admin,
        tool,
        tool_owner_cap,
        tool_cashier_cap,
        agent_id,
        vertex,
    ) = {
        let ctx = scenario.ctx();
        let clock = clock::create_for_testing(ctx);
        let mut agent_registry = agent_registry::new_registry(ctx);
        let agent = agent_registry::create_agent(&mut agent_registry, ctx);
        let agent_id = object::id(&agent);
        let mut wallet = accounting::new_wallet_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, agent_id, ctx);
        accounting::deposit(
            &mut wallet,
            coin::mint_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(1_000, ctx),
            ctx,
        );
        let (mut cashier, operator_cap, settlement_cap) =
            accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, ctx);
        accounting::set_credit_rate(&operator_cap, &mut cashier, 2);
    let (tool_fqn, tool_module, tool_witness_id, output, output_ports) = if (operation == b"register") {
            (
                register::fqn(),
                b"register".to_ascii_string(),
            register::tool_witness_id(&cashier),
            b"registered",
            vector[object_port(b"binding_id")],
            )
        } else if (operation == b"charge") {
            (
                charge::fqn(),
                b"charge".to_ascii_string(),
            charge::tool_witness_id(&cashier),
            b"charged",
            vector[],
            )
        } else if (operation == b"authorize") {
            (
                authorize::fqn(),
                b"authorize".to_ascii_string(),
            authorize::tool_witness_id(&cashier),
            b"authorized",
            vector[data_port(b"binding_id")],
            )
        } else {
            assert!(operation == b"revoke", 1);
            (
                revoke::fqn(),
                b"revoke".to_ascii_string(),
            revoke::tool_witness_id(&cashier),
            b"revoked",
            vector[],
            )
        };
        let dag_tool_fqn = copy tool_fqn;
        let mut tool_registry = tool_registry::new_for_test(ctx);
        let registry_admin = tool_registry::admin_cap_for_testing(&tool_registry, ctx);
        tool_registry.set_us_collateral_to_lock(&registry_admin, 5_000);
        let mut pay_with = coin::mint_for_testing<US>(5_000, ctx);
        let (tool, tool_owner_cap, tool_cashier_cap) =
            tool_registry.register_on_chain_tool_with_workflow_authorization_cap(
                @agent_api_test_coin,
                tool_module,
                tool_fqn,
                b"Agent API lifecycle wrapper test",
            schema(output, output_ports),
                10_000,
                tool_witness_id,
                0,
                &mut pay_with,
                &clock,
                ctx,
            );
        pay_with.destroy_zero();
        let vertex = graph::vertex_from_string(TOOL_VERTEX.to_ascii_string());
        let (mut dag, mut dag_owner) = dag::new_with_owner_cap(ctx);
        tool_registry.add_vertex_to_dag(
            &mut dag,
            &mut dag_owner,
            copy vertex,
            graph::vertex_on_chain(dag_tool_fqn),
        );
        let dag = dag
            .with_entry_port(
                copy vertex,
                graph::input_port_from_string(b"0".to_ascii_string()),
            )
            .with_entry_port(
                copy vertex,
                graph::input_port_from_string(b"1".to_ascii_string()),
            );
        dag::finalize(dag, dag_owner);
        (
            clock,
            agent_registry,
            agent,
            wallet,
            cashier,
            operator_cap,
            settlement_cap,
            tool_registry,
            registry_admin,
            tool,
            tool_owner_cap,
            tool_cashier_cap,
            agent_id,
            vertex,
        )
    };
    let mut agent_registry = agent_registry;
    let mut agent = agent;
    let wallet = wallet;
    let tool_registry = tool_registry;

    scenario.next_tx(OWNER);
    let dag: DAG = scenario.take_immutable();
    let (task_id, leader_registry, leader_cap) = {
        let ctx = scenario.ctx();
        let (leader_registry, leader_cap) =
            test_utils::new_primary_leader_for_testing(&clock, ctx);
        let network_id = leader_cap.what_for();
        let skill_id = agent_registry::register_skill(
            &mut agent_registry,
            &mut agent,
            &tool_registry,
            &dag,
            b"Agent API wrapper fixture",
            vector[],
            payment_interface::payment_policy_user_funded(),
            agent_interface::schedule_once(),
            vector[],
            ctx,
        );
        let mut authorization_bindings = vec_map::empty();
        authorization_bindings.insert(vertex, object::id(&wallet));
        let config = agent_interface::new_agent_execution_config(
            agent_id,
            network_id,
            graph::default_entry_group(),
            graph::inputs_to_begin_execution(
                vector[copy vertex, copy vertex],
                vector[
                    graph::input_port_from_string(b"0".to_ascii_string()),
                    graph::input_port_from_string(b"1".to_ascii_string()),
                ],
                vector[
                    data::one(data::inline_data_value(b"\"binding\"")),
                    data::one(data::inline_data_value(b"1")),
                ],
            ),
            skill_id,
            option::none(),
            authorization_bindings,
            ctx,
        );
        let (mut task, task_pointer) = scheduler::new_user_task(
            &agent_registry,
            &dag,
            &tool_registry,
            &agent,
            config,
            coin::mint_for_testing<SUI>(1_000_000, ctx),
            OWNER,
            1_000_000,
            task::continue_on_failure(),
            ctx,
        );
        let (authority, authority_cap) = runtime_authority::new_for_testing(ctx);
        scheduler::schedule(
            &authority,
            &mut task,
            clock.timestamp_ms(),
            option::none(),
            20,
            ctx,
        );
        runtime_authority::destroy_for_testing(authority, authority_cap);
        let task_id = object::id(&task);
        scheduler::share(task);
        destroy(task_pointer);
        (task_id, leader_registry, leader_cap)
    };
    test_scenario::return_immutable(dag);
    scenario.next_tx(OWNER);
    let dag: DAG = scenario.take_immutable();
    let mut task: Task = scenario.take_shared_by_id(task_id);
    let mut leader_registry = leader_registry;
    let (authorization, requirements, result, result_id, input_hash) = {
        let ctx = scenario.ctx();
        let occurrence_id = scheduler::advertised_occurrence(&task).destroy_some().occurrence_id();
        let leader_seed = std::bcs::to_bytes(
            &task::derive_execution_id(object::id(&task), occurrence_id),
        );
        ensure_admission_leader_selected(
            &mut leader_registry,
            &leader_cap,
            &leader_seed,
            &clock,
            ctx,
        );
        let mut execution = scheduler::admit_next_with_gas_for_testing(
            &mut task,
            &dag,
            &agent_registry,
            &tool_registry,
            &leader_registry,
            &leader_cap,
            occurrence_id,
            0,
            &clock,
            ctx,
        );
        let runtime_vertex = graph::runtime_vertex_plain_from_vertex(vertex);
        invocation_adapter::authorize_for_testing(&dag, &mut execution, runtime_vertex);
        execution::start_execution_for_testing(
            &dag,
            &mut execution,
            &leader_registry,
            &clock,
            ctx,
        );
        let network_auth = network_auth::new_for_testing(ctx);
        let (worksheet, stamp) =
            execution_submission::prepare_tool_result_submission_worksheet_for_testing(
                &dag,
                &agent_registry,
                &tool_registry,
                &network_auth,
                &leader_registry,
                &mut execution,
                &leader_cap,
                0,
                &clock,
                ctx,
            );
        let authorization = execution_submission::release_vertex_authorization_for_onchain_walk_for_testing(
            &dag,
            &mut execution,
            &worksheet,
            &stamp,
            &leader_cap,
            0,
        );
        let (requirements, result) =
            execution_submission::create_on_chain_tool_result_for_walk_for_testing(
                &dag,
                &mut execution,
                &tool_registry,
                worksheet,
                &stamp,
                &leader_cap,
                &leader_registry,
                0,
                runtime_vertex,
                ctx,
            );
        let result_id = result_api::id(&result);
        let input_hash = result_api::input_commitment(&result);
        network_auth::destroy_registry_for_testing(network_auth);
        execution::destroy_execution_payment_with_locks_for_testing(&mut execution);
        destroy(execution);
        (authorization, requirements, result, result_id, input_hash)
    };
    test_scenario::return_shared(task);
    test_scenario::return_immutable(dag);

    tool_registry::destroy_tool_for_testing(tool);
    tool_registry::destroy_admin_cap_for_testing(registry_admin);
    tool_registry::destroy_registry_for_testing(tool_registry);
    destroy(tool_owner_cap);
    destroy(tool_cashier_cap);
    agent_registry::destroy_agent_record_for_testing(&mut agent_registry, agent_id);
    agent_registry::destroy_registry_for_testing(agent_registry);
    destroy(leader_registry);
    destroy(leader_cap);
    destroy(clock);

    AuthorizedCall {
        agent,
        wallet,
        cashier,
        operator_cap,
        settlement_cap,
        authorization,
        requirements,
        result,
        result_id,
        binding_id: binding_id(),
        input_hash,
    }
}

fun finish_call(scenario: &mut test_scenario::Scenario, result_id: ID) {
    scenario.next_tx(OWNER);
    let result: OnchainToolResult = scenario.take_shared_by_id(result_id);
    assert!(result_api::is_finalized(&result), 10);
    destroy(result);
}

fun query_meta_schema(): meta_schema::MetaSchema {
    meta_schema::new(
        vector[data_port(b"binding_id"), data_port(b"payload")],
        vector[
            meta_schema::output_variant_schema(
                b"ok",
                vector[data_port(b"result"), data_port(b"cumulative_usage")],
            ),
            meta_schema::output_variant_schema(b"err", vector[data_port(b"reason")]),
        ],
    )
}

#[test]
fun authorize_binding_id_data_output_connects_to_offchain_query() {
    let mut ctx = sui::tx_context::dummy();
    let clock = clock::create_for_testing(&mut ctx);
    let mut tool_registry = tool_registry::new_for_test(&mut ctx);
    let registry_admin = tool_registry::admin_cap_for_testing(&tool_registry, &mut ctx);
    tool_registry.set_us_collateral_to_lock(&registry_admin, 5_000);
    let (cashier, operator_cap, settlement_cap) =
        accounting::new_cashier_for_testing<agent_api_test_coin::test_coin::TEST_COIN>(OWNER, &mut ctx);
    let mut collateral = coin::mint_for_testing<US>(10_000, &mut ctx);
    let (authorize_tool, authorize_owner, authorize_cashier) =
        tool_registry.register_on_chain_tool_with_workflow_authorization_cap(
            @agent_api_test_coin,
            b"authorize".to_ascii_string(),
            authorize::fqn(),
            b"Agent API authorization Tool schema test",
            schema(b"authorized", vector[data_port(b"binding_id")]),
            10_000,
            authorize::tool_witness_id(&cashier),
            0,
            &mut collateral,
            &clock,
            &mut ctx,
        );
    let (query_tool, query_owner, query_cashier) = tool_registry.register_off_chain_tool(
        b"xyz.taluslabs.agent_api.query@1".to_ascii_string(),
        b"https://example.invalid",
        b"Agent API query schema test",
        query_meta_schema(),
        10_000,
        0,
        &mut collateral,
        &clock,
        &mut ctx,
    );
    collateral.destroy_zero();

    let authorize_vertex = graph::vertex_from_string(b"authorize".to_ascii_string());
    let query_vertex = graph::vertex_from_string(b"query".to_ascii_string());
    let (mut dag, mut dag_owner) = dag::new_with_owner_cap(&mut ctx);
    tool_registry.add_vertex_to_dag(
        &mut dag,
        &mut dag_owner,
        copy authorize_vertex,
        graph::vertex_on_chain(authorize::fqn()),
    );
    tool_registry.add_vertex_to_dag(
        &mut dag,
        &mut dag_owner,
        copy query_vertex,
        graph::vertex_off_chain(b"xyz.taluslabs.agent_api.query@1".to_ascii_string()),
    );
    let dag = dag
        .with_entry_port(
            copy authorize_vertex,
            graph::input_port_from_string(b"0".to_ascii_string()),
        )
        .with_entry_port(
            copy authorize_vertex,
            graph::input_port_from_string(b"1".to_ascii_string()),
        )
        .with_entry_port(
            copy query_vertex,
            graph::input_port_from_string(b"payload".to_ascii_string()),
        )
        .with_edge(
            authorize_vertex,
            graph::output_variant_from_string(b"authorized".to_ascii_string()),
            graph::output_port_from_string(b"binding_id".to_ascii_string()),
            query_vertex,
            graph::input_port_from_string(b"binding_id".to_ascii_string()),
            graph::edge_kind_normal(),
        );
    let mut dag = dag;
    dag::finalize_for_testing(&mut dag, dag_owner);
    destroy(dag);

    tool_registry::destroy_tool_for_testing(authorize_tool);
    tool_registry::destroy_tool_for_testing(query_tool);
    destroy(authorize_owner);
    destroy(authorize_cashier);
    destroy(query_owner);
    destroy(query_cashier);
    tool_registry::destroy_admin_cap_for_testing(registry_admin);
    tool_registry::destroy_registry_for_testing(tool_registry);
    destroy(cashier);
    destroy(operator_cap);
    destroy(settlement_cap);
    destroy(clock);
}

#[test]
fun register_execute_uses_framework_authorization_and_finalizes() {
    let mut scenario = test_scenario::begin(OWNER);
    let result_id = {
        let AuthorizedCall {
            agent,
            wallet,
            cashier,
            operator_cap,
            settlement_cap,
            authorization,
            requirements,
            result,
            result_id,
            binding_id: _,
            input_hash: _,
        } = new_authorized_call(&mut scenario, b"register");
        let ctx = scenario.ctx();
        let mut wallet = wallet;
        let mut cashier = cashier;
        register::execute(
            authorization,
            requirements,
            result,
            &mut wallet,
            &mut cashier,
            false,
            b"".to_ascii_string(),
            ctx,
        );
        let registration_events =
            sui::event::events_by_type<accounting::RegistrationEvent<agent_api_test_coin::test_coin::TEST_COIN>>();
        assert!(registration_events.length() == 1, 12);
        let binding_id =
            accounting::registration_binding_id_for_testing(&registration_events[0]);
        let stored_owner_public_key =
            accounting::binding_owner_public_key_for_testing(&cashier, binding_id);
        let emitted_owner_public_key =
            accounting::registration_owner_public_key_for_testing(&registration_events[0]);
        assert!(stored_owner_public_key.is_empty(), 17);
        assert!(emitted_owner_public_key.is_empty(), 18);
        let (active, refunded, rate, charged, credits, _, reserve) =
            accounting::binding_state_for_testing(&cashier, binding_id);
        assert!(active && !refunded && rate == 2 && charged == 0 && credits == 0 && reserve == 0, 11);
        let (tag, payload) = tagged_output::into_parts(register::registered_output(binding_id));
        assert!(tag == b"registered", 13);
        let mut expected_binding_id_json = b"\"0x".to_ascii_string();
        expected_binding_id_json.append(object::id_to_address(&binding_id).to_ascii_string());
        expected_binding_id_json.append(b"\"".to_ascii_string());
        assert!(
            data::inline_data_bytes(payload.get(&b"binding_id")).destroy_some()
                == expected_binding_id_json.into_bytes(),
            14
        );
        let _ = balance::destroy_for_testing(agent_interface::destroy_agent_for_testing(agent));
        destroy(wallet);
        destroy(cashier);
        destroy(operator_cap);
        destroy(settlement_cap);
        result_id
    };
    finish_call(&mut scenario, result_id);
    scenario.end();
}

#[test]
fun register_execute_accepts_enabled_owner_key_scalar() {
    let mut scenario = test_scenario::begin(OWNER);
    let result_id = {
        let AuthorizedCall {
            agent,
            wallet,
            cashier,
            operator_cap,
            settlement_cap,
            authorization,
            requirements,
            result,
            result_id,
            binding_id: _,
            input_hash: _,
        } = new_authorized_call(&mut scenario, b"register");
        let ctx = scenario.ctx();
        let mut wallet = wallet;
        let mut cashier = cashier;
        register::execute(
            authorization,
            requirements,
            result,
            &mut wallet,
            &mut cashier,
            true,
            b"000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F"
                .to_ascii_string(),
            ctx,
        );
        let registration_events =
            sui::event::events_by_type<accounting::RegistrationEvent<agent_api_test_coin::test_coin::TEST_COIN>>();
        assert!(registration_events.length() == 1, 15);
        let binding_id =
            accounting::registration_binding_id_for_testing(&registration_events[0]);
        let expected_owner_public_key: vector<u8> = vector[
            0u8, 1u8, 2u8, 3u8, 4u8, 5u8, 6u8, 7u8,
            8u8, 9u8, 10u8, 11u8, 12u8, 13u8, 14u8, 15u8,
            16u8, 17u8, 18u8, 19u8, 20u8, 21u8, 22u8, 23u8,
            24u8, 25u8, 26u8, 27u8, 28u8, 29u8, 30u8, 31u8,
        ];
        let stored_owner_public_key =
            accounting::binding_owner_public_key_for_testing(&cashier, binding_id);
        let emitted_owner_public_key =
            accounting::registration_owner_public_key_for_testing(&registration_events[0]);
        assert!(stored_owner_public_key == expected_owner_public_key, 17);
        assert!(emitted_owner_public_key == expected_owner_public_key, 18);
        let (active, refunded, rate, charged, credits, _, reserve) =
            accounting::binding_state_for_testing(&cashier, binding_id);
        assert!(active && !refunded && rate == 2 && charged == 0 && credits == 0 && reserve == 0, 16);
        let _ = balance::destroy_for_testing(agent_interface::destroy_agent_for_testing(agent));
        destroy(wallet);
        destroy(cashier);
        destroy(operator_cap);
        destroy(settlement_cap);
        result_id
    };
    finish_call(&mut scenario, result_id);
    scenario.end();
}

#[test]
fun charge_execute_moves_funds_only_into_its_refundable_reserve() {
    let mut scenario = test_scenario::begin(OWNER);
    let result_id = {
        let AuthorizedCall {
            agent,
            wallet,
            cashier,
            operator_cap,
            settlement_cap,
            authorization,
            requirements,
            result,
            result_id,
            binding_id,
            input_hash: _,
        } = new_authorized_call(&mut scenario, b"charge");
        let ctx = scenario.ctx();
        let mut wallet = wallet;
        let mut cashier = cashier;
        let binding_id = accounting::register(&wallet, &mut cashier, false, vector[], ctx);
        charge::execute(
            authorization,
            requirements,
            result,
            &mut wallet,
            &mut cashier,
            binding_id,
            40,
            ctx,
        );
        let (_, _, _, charged, credits, _, reserve) =
            accounting::binding_state_for_testing(&cashier, binding_id);
        assert!(accounting::wallet_balance_for_testing(&wallet) == 960, 20);
        assert!(charged == 40 && credits == 80 && reserve == 40, 21);
        let _ = balance::destroy_for_testing(agent_interface::destroy_agent_for_testing(agent));
        destroy(wallet);
        destroy(cashier);
        destroy(operator_cap);
        destroy(settlement_cap);
        result_id
    };
    finish_call(&mut scenario, result_id);
    scenario.end();
}

#[test]
fun authorize_execute_issues_one_grant_for_the_result_commitment() {
    let mut scenario = test_scenario::begin(OWNER);
    let result_id = {
        let AuthorizedCall {
            agent,
            wallet,
            cashier,
            operator_cap,
            settlement_cap,
            authorization,
            requirements,
            result,
            result_id,
            binding_id,
            input_hash,
        } = new_authorized_call(&mut scenario, b"authorize");
        let ctx = scenario.ctx();
        let mut wallet = wallet;
        let mut cashier = cashier;
        let binding_id = accounting::register(&wallet, &mut cashier, false, vector[], ctx);
        accounting::charge(&mut wallet, &mut cashier, binding_id, 10);
        authorize::execute(
            authorization,
            requirements,
            result,
            &mut wallet,
            &mut cashier,
            binding_id,
            TOOL_VERTEX,
            0,
            0,
            b"query",
            input_hash,
            ctx,
        );
        let (tag, payload) = tagged_output::into_parts(authorize::authorized_output(binding_id));
        assert!(tag == b"authorized", 31);
        let mut expected_binding_id_json = b"\"0x".to_ascii_string();
        expected_binding_id_json.append(object::id_to_address(&binding_id).to_ascii_string());
        expected_binding_id_json.append(b"\"".to_ascii_string());
        assert!(
            data::inline_data_bytes(payload.get(&b"binding_id")).destroy_some()
                == expected_binding_id_json.into_bytes(),
            32
        );
        assert!(
            sui::event::events_by_type<accounting::AuthorizationEvent<agent_api_test_coin::test_coin::TEST_COIN>>().length() == 1,
            30,
        );
        let _ = balance::destroy_for_testing(agent_interface::destroy_agent_for_testing(agent));
        destroy(wallet);
        destroy(cashier);
        destroy(operator_cap);
        destroy(settlement_cap);
        result_id
    };
    finish_call(&mut scenario, result_id);
    scenario.end();
}

#[test]
fun revoke_execute_disables_the_authorized_binding_and_finalizes() {
    let mut scenario = test_scenario::begin(OWNER);
    let result_id = {
        let AuthorizedCall {
            agent,
            wallet,
            cashier,
            operator_cap,
            settlement_cap,
            authorization,
            requirements,
            result,
            result_id,
            binding_id,
            input_hash: _,
        } = new_authorized_call(&mut scenario, b"revoke");
        let ctx = scenario.ctx();
        let mut wallet = wallet;
        let mut cashier = cashier;
        let binding_id = accounting::register(&wallet, &mut cashier, false, vector[], ctx);
        revoke::execute(
            authorization,
            requirements,
            result,
            &mut wallet,
            &mut cashier,
            binding_id,
            ctx,
        );
        let (active, _, _, _, _, _, _) =
            accounting::binding_state_for_testing(&cashier, binding_id);
        assert!(!active, 40);
        assert!(
            sui::event::events_by_type<accounting::RevokedEvent<agent_api_test_coin::test_coin::TEST_COIN>>().length() == 1,
            41,
        );
        let _ = balance::destroy_for_testing(agent_interface::destroy_agent_for_testing(agent));
        destroy(wallet);
        destroy(cashier);
        destroy(operator_cap);
        destroy(settlement_cap);
        result_id
    };
    finish_call(&mut scenario, result_id);
    scenario.end();
}
