mod keygen;
extern crate alloc;
use alloc::vec::Vec;
use core::cell::RefCell;
use std::{fs, marker::PhantomData};

use crypto::{
    default::{DefaultCipherSuite, DefaultEngine},
    keystore::fs_keystore::Store,
    Rng, UserId,
};
use crypto_ffi::Ffi as CryptoFfi;
use device_ffi::FfiDevice as DeviceFfi;
use envelope_ffi::Ffi as EnvelopeFfi;
use idam_ffi::Ffi as IdamFfi;
use perspective_ffi::FfiPerspective as PerspectiveFfi;
use policy_compiler::Compiler;
use policy_lang::lang::parse_policy_document;
use policy_vm::{
    ffi::{FfiModule, ModuleSchema},
    Machine,
};
use runtime::{
    storage::memory::MemStorageProvider,
    vm_action, vm_effect,
    vm_policy::{testing::TestFfiEnvelope, VmPolicy},
    ClientState, Engine, FfiCallable, StorageProvider,
};
use tempfile::tempdir;
use test_log::test;

use crate::{
    tests::keygen::{KeyBundle, MinKeyBundle, PublicKeys},
    ClientFactory, Model, ModelClient, ModelEngine, ModelError, ProxyClientId, ProxyGraphId,
    RuntimeModel,
};

// Policy loaded from md file.
const FFI_POLICY: &str = include_str!("./ffi-policy.md");
const BASIC_POLICY: &str = include_str!("./basic-policy.md");

// NOTE: In actual usage, we would only have one client factory per
// implementation, I included two here for testing purposes.
struct BasicClientFactory {
    machine: Machine,
}

impl BasicClientFactory {
    fn new(policy_doc: &str) -> Result<Self, ModelError> {
        let ffi_schema: &[ModuleSchema<'static>] = &[TestFfiEnvelope::SCHEMA];

        let policy_ast = parse_policy_document(policy_doc)?;
        // Create policy machine
        let module = Compiler::new(&policy_ast)
            .ffi_modules(ffi_schema)
            .compile()?;
        let machine = Machine::from_module(module).expect("should be able to load compiled module");

        Ok(Self { machine })
    }
}

// BasicClientFactory doesn't use signing keys, we add an empty struct to satisfy
// the requirement.
#[derive(Default)]
struct EmptyKeys;

// The `BasicClientFactory` will create clients that have the minimal configuration
// necessary to to satisfy the policy_vm. The main part being, the use of the
// `TestFfiEnvelope` ffi needed to satisfy requirements in the policy envelope.
impl ClientFactory for BasicClientFactory {
    type Engine = ModelEngine<DefaultEngine>;
    type StorageProvider = MemStorageProvider;
    type PublicKeys = EmptyKeys;
    type Args = ();

    fn create_client(&mut self, (): ()) -> ModelClient<BasicClientFactory> {
        let (eng, _) = DefaultEngine::from_entropy(Rng);

        // Configure testing FFIs
        let ffis: Vec<Box<dyn FfiCallable<DefaultEngine> + Send + 'static>> =
            vec![Box::from(TestFfiEnvelope {
                user: UserId::random(&mut Rng),
            })];

        let policy = VmPolicy::new(self.machine.clone(), eng, ffis).expect("should create policy");
        let engine = ModelEngine::new(policy);
        let provider = MemStorageProvider::new();

        ModelClient {
            state: RefCell::new(ClientState::new(engine, provider)),
            public_keys: EmptyKeys,
        }
    }
}

struct FfiClientFactory {
    machine: Machine,
}

impl FfiClientFactory {
    fn new(policy_doc: &str) -> Result<Self, ModelError> {
        let ffi_schema: &[ModuleSchema<'static>] = &[
            DeviceFfi::SCHEMA,
            EnvelopeFfi::SCHEMA,
            PerspectiveFfi::SCHEMA,
            CryptoFfi::<Store>::SCHEMA,
            IdamFfi::<Store>::SCHEMA,
        ];

        let policy_ast = parse_policy_document(policy_doc)?;
        // Create policy machine
        let module = Compiler::new(&policy_ast)
            .ffi_modules(ffi_schema)
            .compile()?;
        let machine = Machine::from_module(module).expect("should be able to load compiled module");

        Ok(Self { machine })
    }
}

// The FfiClientFactory uses signing keys in it's envelope, thus requires
// supporting FFIs.
impl ClientFactory for FfiClientFactory {
    type Engine = ModelEngine<DefaultEngine>;
    type StorageProvider = MemStorageProvider;
    type PublicKeys = PublicKeys<DefaultCipherSuite>;
    type Args = ();

    fn create_client(&mut self, (): ()) -> ModelClient<FfiClientFactory> {
        // Setup keystore
        let temp_dir = tempdir().expect("should create temp directory");
        let root = temp_dir.into_path().join("client");
        assert!(
            !root.try_exists().expect("should create root path"),
            "duplicate client name"
        );
        let mut store = {
            let path = root.join("keystore");
            fs::create_dir_all(&path).expect("should create directory");
            Store::open(&path).expect("should create keystore")
        };

        // Generate key bundle
        let (mut eng, _) = DefaultEngine::from_entropy(Rng);
        let bundle =
            KeyBundle::generate(&mut eng, &mut store).expect("unable to generate `KeyBundle`");
        let public_keys = bundle
            .public_keys(&mut eng, &store)
            .expect("unable to generate public keys");

        // Configure FFIs
        let ffis: Vec<Box<dyn FfiCallable<DefaultEngine> + Send + 'static>> = vec![
            Box::from(DeviceFfi::new(bundle.user_id)),
            Box::from(EnvelopeFfi),
            Box::from(PerspectiveFfi),
            Box::from(CryptoFfi::new(
                store.try_clone().expect("should clone key store"),
            )),
            Box::from(IdamFfi::new(store)),
        ];

        let policy = VmPolicy::new(self.machine.clone(), eng, ffis).expect("should create policy");
        let engine = ModelEngine::new(policy);
        let provider = MemStorageProvider::new();

        ModelClient {
            state: RefCell::new(ClientState::new(engine, provider)),
            public_keys,
        }
    }
}

struct IdentityClientFactory<E, SP, PK>(PhantomData<(E, SP, PK)>);

/// A client factory that just passes through a client.
impl<E, SP, PK> ClientFactory for IdentityClientFactory<E, SP, PK>
where
    E: Engine,
    SP: StorageProvider,
{
    type Engine = E;
    type StorageProvider = SP;
    type PublicKeys = PK;
    type Args = ModelClient<Self>;

    fn create_client(&mut self, client: Self::Args) -> ModelClient<Self> {
        client
    }
}

/// We use an enum to automatically define unique values for different users.
enum User {
    A,
    B,
}

impl From<User> for ProxyClientId {
    fn from(value: User) -> Self {
        Self(value as u64)
    }
}

/// We use an enum to automatically define unique values for different graphs.
enum Graph {
    X,
    Y,
}

impl From<Graph> for ProxyGraphId {
    fn from(value: Graph) -> Self {
        Self(value as u64)
    }
}

// To perform a simple smoke test with a minimally configured client, we will
// create a single "basic" client, with a graph and add actions to it, then inspect
// each effect we get back. The basic clients are configured to satisfy the
// requirements of the basic-policy.md.
#[test]
fn should_create_basic_client_and_add_commands() {
    // Create our client factory, this will be responsible for creating all our clients.
    let basic_clients =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model instance with our client factory.
    let mut test_model = RuntimeModel::new(basic_clients);

    // Create a single client
    test_model
        .add_client(User::A)
        .expect("Should create a client");

    let nonce = 1;
    // Add a graph to our client
    test_model
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect("Should create a graph");

    // Issue our first action, it will create a fact in the FactDB.
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(create_action(3)))
        .expect("Should return effect");
    // Observe that we get back a single effect.
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 3 })]);

    // Issue an action to increment the value by one.
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(1)))
        .expect("Should return effect");
    // Again we check that we receive a single effect,
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 4 })]);

    // We issue another action to increment the count by five this time,
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(5)))
        .expect("Should return effect");
    // again we receive a single effect back as expected,
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 9 })]);

    // Now we issue an action to decrease the value by one.
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(decrement(1)))
        .expect("Should return effect");
    // We receive a single effect back as expected,
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 8 })]);
}

// To perform a smoke test with fully configured client, we will
// create a single "ffi" client, with a graph and add actions to it, inspecting
// each effect we get back. In these tests, ffi clients are configured to satisfy
// the fuller requirements of the ffi-policy.md.
#[test]
fn should_create_client_with_ffi_and_add_commands() {
    // Create our client factory, this will be responsible for creating all our clients.
    let ffi_clients = FfiClientFactory::new(FFI_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::new(ffi_clients);

    // Create a single client
    test_model
        .add_client(User::A)
        .expect("Should create a client");

    // Retrieve the public keys for our client.
    let client_public_keys = test_model
        .get_public_keys(User::A)
        .expect("could not get public keys");

    // Pull off the public identity key.
    let client_ident_pk =
        postcard::to_allocvec(&client_public_keys.ident_pk).expect("should get ident pk");
    // Pull off the public signing key.
    let client_sign_pk =
        postcard::to_allocvec(&client_public_keys.sign_pk).expect("should get sign pk");

    let nonce = 1;
    // Create a graph for client A. The init command in the ffi policy
    // required the public signing key.
    test_model
        .new_graph(
            Graph::X,
            User::A,
            vm_action!(init(nonce, client_sign_pk.clone())),
        )
        .expect("Should create a graph");

    // Add client keys to the fact db. Here we call the `add_user_keys` action
    // that takes in as arguments the public identity key and public signing key.
    test_model
        .action(
            User::A,
            Graph::X,
            vm_action!(add_user_keys(
                client_ident_pk.clone(),
                client_sign_pk.clone()
            )),
        )
        .expect("should add user");

    // Issue the create action, it will create a fact in the FactDB with the value
    // we pass in. Note that we no longer need to pass in the signing key, the ffi
    // policy commands will be responsible for looking up signing information.
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(create_action(3)))
        .expect("Should return effect");
    // Observe that we get back a single effect.
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 3 })]);

    // Issue an action to increment the value by one.
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(1)))
        .expect("Should return effect");
    // Again we check that we receive a single effect,
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 4 })]);

    // We issue another action to increment the count by five this time,
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(5)))
        .expect("Should return effect");
    // again we receive a single effect back as expected,
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 9 })]);

    // Now we issue an action to decrease the value by one.
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(decrement(1)))
        .expect("Should return effect");
    // We receive a single effect back as expected,
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 8 })]);
}

// Client proxy IDs within the model must be unique, we enforce this by returning an
// error if a duplicate ID is used.
#[test]
fn should_fail_duplicate_client_ids() {
    // Create our client factory, this will be responsible for creating all our clients.
    let basic_clients =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::<_, User, Graph>::new(basic_clients);

    test_model
        .add_client(User::A)
        .expect("Should create a client");

    // Creating a second client with an id of one will cause an error.
    test_model
        .add_client(User::A)
        .expect_err("Should fail client creation if proxy_id is reused");
}

// Graph proxy IDs within the model must be unique, we enforce this by returning an
// error if a duplicate ID is used.
#[test]
fn should_fail_duplicate_graph_ids() {
    // Create our client factory, this will be responsible for creating all our clients.
    let basic_clients =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::new(basic_clients);

    // Create a client
    test_model
        .add_client(User::A)
        .expect("Should create a client");

    let nonce = 1;
    // Create the first graph with an id of one
    test_model
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect("Should create a graph");

    let nonce = 2;
    // Creating a second graph with a proxy id of one will cause an error.
    test_model
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect_err("Should fail graph creation if proxy_id is reused");
}

// The client should allow the use of multiple graphs on a single client, this
// use case could be thought of a single user that belongs to multiple teams.
#[test]
fn should_allow_multiple_graphs() {
    // Create our client factory, this will be responsible for creating all our clients.
    let basic_clients =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::new(basic_clients);

    test_model
        .add_client(User::A)
        .expect("Should create a client");

    let nonce = 1;
    test_model
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect("Should create a graph");

    let nonce = 2;
    test_model
        .new_graph(Graph::Y, User::A, vm_action!(init(nonce)))
        .expect("Should support the ability to add multiple graphs");
}

// Clients should be able to sync data between each other. This test creates two
// clients, issues action on each client and syncs with the other client multiple
// times. We verify that the data is correct after every sync.
#[test]
fn should_sync_ffi_clients() {
    // Create our client factory, this will be responsible for creating all our clients.
    let ffi_clients = FfiClientFactory::new(FFI_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::new(ffi_clients);

    // Create our first client.
    test_model
        .add_client(User::A)
        .expect("Should create a client");

    // Retrieve the public keys for the client.
    let client_one_public_keys = test_model
        .get_public_keys(User::A)
        .expect("could not get public keys");

    // Pull off the public identity key of our first client.
    let client_one_ident_pk =
        postcard::to_allocvec(&client_one_public_keys.ident_pk).expect("should get ident pk");
    // Pull off the public signing key of our first client.
    let client_one_sign_pk =
        postcard::to_allocvec(&client_one_public_keys.sign_pk).expect("should get sign pk");

    let nonce = 1;
    // Create a graph for client A. The init command in the ffi policy
    // required the public signing key.
    test_model
        .new_graph(
            Graph::X,
            User::A,
            vm_action!(init(nonce, client_one_sign_pk.clone())),
        )
        .expect("Should create a graph");

    // Add client's keys to the fact db. Here we call the `add_user_keys` action
    // that takes in as arguments the public identity key and public signing key.
    test_model
        .action(
            User::A,
            Graph::X,
            vm_action!(add_user_keys(
                client_one_ident_pk.clone(),
                client_one_sign_pk.clone()
            )),
        )
        .expect("should add user");

    // Issue the create action, it will create a fact in the FactDB with the value
    // we pass in. Note that we no longer need to pass in the signing key, the ffi
    // policy commands will be responsible for looking up signing information.
    test_model
        .action(User::A, Graph::X, vm_action!(create_action(3)))
        .expect("Should return effect");

    // Issue an action to increment the value by one.
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(1)))
        .expect("Should return effect");
    // Check that we receive a single effect,
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 4 })]);

    // Create our second client.
    test_model
        .add_client(User::B)
        .expect("Should create a client");

    // Retrieve the public keys for our second client.
    let client_two_public_keys = test_model
        .get_public_keys(User::B)
        .expect("could not get public keys");

    // Pull off the public identity key of our second client.
    let client_two_ident_pk =
        postcard::to_allocvec(&client_two_public_keys.ident_pk).expect("should get ident pk");
    // Pull off the public signing key of our second client.
    let client_two_sign_pk =
        postcard::to_allocvec(&client_two_public_keys.sign_pk).expect("should get sign pk");

    // Sync client B with client A (A -> B). Syncs are unidirectional, client
    // B will receive all the new commands it doesn't yet know about from client
    // A. At this stage of the test, that's the init, add_user_keys, create, and
    // increment commands.
    test_model
        .sync(Graph::X, User::A, User::B)
        .expect("Should sync clients");

    // Add our client's public keys to the graph.
    test_model
        .action(
            User::B,
            Graph::X,
            vm_action!(add_user_keys(
                client_two_ident_pk.clone(),
                client_two_sign_pk.clone()
            )),
        )
        .expect("should add user");

    // After our sync and user key are added, we can increase the count on our
    // second client
    let effects = test_model
        .action(User::B, Graph::X, vm_action!(increment(2)))
        .expect("Should return effect");
    // Check that we get back a single effect
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 6 })]);

    // Perform another increment action
    let effects = test_model
        .action(User::B, Graph::X, vm_action!(increment(3)))
        .expect("Should return effect");
    // check that we get back our single effect
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 9 })]);

    // Sync client A with client B (B -> A). Client A will receive both of
    // our increment command as well as client B's add_user_keys command.
    test_model
        .sync(Graph::X, User::B, User::A)
        .expect("Should sync clients");

    // Increment client B after syncing with client A
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(4)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 13 })]);

    // Sync client B with client A (A -> B)
    test_model
        .sync(Graph::X, User::A, User::B)
        .expect("Should sync clients");

    // Increment client B after syncing with client A
    let effects = test_model
        .action(User::B, Graph::X, vm_action!(increment(5)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 18 })]);
}

// Clients should be able to sync data between each other. This test creates two
// clients, issues action on each client and syncs the factDB with the other client
// multiple times. We verify that the data is correct after every sync.
#[test]
fn should_sync_basic_clients() {
    // Create our client factory, this will be responsible for creating all our clients.
    let basic_clients =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::new(basic_clients);

    // Create our first client.
    test_model
        .add_client(User::A)
        .expect("Should create a client");

    let nonce = 1;
    // Create a graph for client A.
    test_model
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect("Should create a graph");

    // Issue the create action, it will create a fact in the FactDB with the value
    // we pass in.
    test_model
        .action(User::A, Graph::X, vm_action!(create_action(3)))
        .expect("Should return effect");

    // Issue an action to increment the value by one.
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(1)))
        .expect("Should return effect");
    // Check that we receive a single effect,
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 4 })]);

    // Create our second client.
    test_model
        .add_client(User::B)
        .expect("Should create a client");

    // Sync client B with client A (A -> B). Syncs are unidirectional, client
    // B will receive all the new commands it doesn't yet know about from client
    // A. At this stage of the test, that's the init, create, and
    // increment commands.
    test_model
        .sync(Graph::X, User::A, User::B)
        .expect("Should sync clients");

    // Increment client B after syncing with client A.
    let effects = test_model
        .action(User::B, Graph::X, vm_action!(increment(2)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 6 })]);

    let effects = test_model
        .action(User::B, Graph::X, vm_action!(increment(3)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 9 })]);

    // Sync client A from client B (B -> A), this has both of the increment
    // commands issued on client B.
    test_model
        .sync(Graph::X, User::B, User::A)
        .expect("Should sync clients");

    // Increment the count of client B after syncing with client A
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(4)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 13 })]);

    // Sync client B with client A (A -> B)
    test_model
        .sync(Graph::X, User::A, User::B)
        .expect("Should sync clients");

    // Increment client B after syncing with client A
    let effects = test_model
        .action(User::B, Graph::X, vm_action!(increment(5)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 18 })]);
}

// In the basic client implementation, the `TestFfiEnvelope` is responsible for
// creating the graph command IDs, this is done in part by the payload. This test
// make sure that duplicate identical payloads produce unique ids, thus syncing all
// the commands.
#[test]
fn should_sync_clients_with_duplicate_payloads() {
    // Create our client factory, this will be responsible for creating all our clients.
    let basic_clients =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::new(basic_clients);

    // Create our first client
    test_model
        .add_client(User::A)
        .expect("Should create a client");

    let nonce = 1;
    // Add a graph to our client
    test_model
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect("Should create a graph");

    // Issue our first action, it will create a fact in the FactDB.
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(create_action(1)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 1 })]);

    // Here we want to issue a series of actions that are all identical.
    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(1)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 2 })]);

    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(1)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 3 })]);

    let effects = test_model
        .action(User::A, Graph::X, vm_action!(increment(1)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 4 })]);

    // Create our second client.
    test_model
        .add_client(User::B)
        .expect("Should create a client");

    // Sync client B with client A (A -> B)
    test_model
        .sync(Graph::X, User::A, User::B)
        .expect("Should sync clients");

    // Now test that all of our commands have been synced and we can increase
    // our count as expected.
    let effects = test_model
        .action(User::B, Graph::X, vm_action!(increment(1)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 5 })]);
}

// We want to be able to use multiple instances of the model simultaneously.
#[test]
fn should_allow_multiple_instances_of_model() {
    // Create our first client factory, this will be responsible for creating all our clients.
    let basic_clients_1 =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model_1 = RuntimeModel::new(basic_clients_1);

    // Create our second client factory.
    let basic_clients_2 =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model_2 = RuntimeModel::new(basic_clients_2);

    // Add a client to the first model
    test_model_1
        .add_client(User::A)
        .expect("Should create a client");

    // Add a client to the second model
    test_model_2
        .add_client(User::A)
        .expect("Should create a client");

    let nonce = 1;
    // Create a graph on the first model client
    test_model_1
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect("Should create a graph");

    // Issue a create action on the first model client.
    let effects = test_model_1
        .action(User::A, Graph::X, vm_action!(create_action(1)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 1 })]);

    // Create a graph on the second model client
    let nonce = 1;
    test_model_2
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect("Should create a graph");

    // Issue an action on the second model client
    let effects = test_model_2
        .action(User::A, Graph::X, vm_action!(create_action(1)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 1 })]);
}

// We want to be able to use multiple instances of the model simultaneously. This
// test verifies that we can do that with a ffi configured clients.
#[test]
fn should_allow_multiple_instances_of_model_with_ffi() {
    // Create our first client factory, this will be responsible for creating all our clients.
    let ffi_clients_1 = FfiClientFactory::new(FFI_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model_1 = RuntimeModel::new(ffi_clients_1);

    // Create our second client factory.
    let ffi_clients_2 = FfiClientFactory::new(FFI_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model_2 = RuntimeModel::new(ffi_clients_2);

    // Add a client to the first model
    test_model_1
        .add_client(User::A)
        .expect("Should create a client");

    // Retrieve the public keys for our client.
    let model_one_public_keys = test_model_1
        .get_public_keys(User::A)
        .expect("should get public keys");

    // Pull off the public identity key.
    let model_one_ident_pk =
        postcard::to_allocvec(&model_one_public_keys.ident_pk).expect("should get ident pk");
    // Pull off the public signing key.
    let model_one_sign_pk =
        postcard::to_allocvec(&model_one_public_keys.sign_pk).expect("should get sign pk");

    let nonce = 1;
    // Create a graph for client A. The init command in the ffi policy
    // required the public signing key.
    test_model_1
        .new_graph(
            Graph::X,
            User::A,
            vm_action!(init(nonce, model_one_sign_pk.clone())),
        )
        .expect("Should create a graph");

    // Add client keys to the fact db. Here we call the `add_user_keys` action
    // that takes in as arguments the public identity key and signing key.
    test_model_1
        .action(
            User::A,
            Graph::X,
            vm_action!(add_user_keys(
                model_one_ident_pk.clone(),
                model_one_sign_pk.clone()
            )),
        )
        .expect("should add user");

    // Issue a create action on the first model client
    let effects = test_model_1
        .action(User::A, Graph::X, vm_action!(create_action(1)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 1 })]);

    // Add a client to the second model
    test_model_2
        .add_client(User::A)
        .expect("Should create a client");

    // Retrieve the public keys for our client.
    let model_two_public_keys = test_model_2
        .get_public_keys(User::A)
        .expect("should get public keys");

    // Pull off the public identity key.
    let model_two_ident_pk =
        postcard::to_allocvec(&model_two_public_keys.ident_pk).expect("should get ident pk");
    // Pull off the public signing key.
    let model_two_sign_pk =
        postcard::to_allocvec(&model_two_public_keys.sign_pk).expect("should get sign_pk");

    let nonce = 1;
    // Create a graph on the second model client
    test_model_2
        .new_graph(
            Graph::X,
            User::A,
            vm_action!(init(nonce, model_two_sign_pk.clone())),
        )
        .expect("Should create a graph");

    // Add client keys to the fact db.
    test_model_2
        .action(
            User::A,
            Graph::X,
            vm_action!(add_user_keys(
                model_two_ident_pk.clone(),
                model_two_sign_pk.clone()
            )),
        )
        .expect("should add user");

    // Issue an action on the second model client
    let effects = test_model_2
        .action(User::A, Graph::X, vm_action!(create_action(1)))
        .expect("Should return effect");
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 1 })]);
}

// To test ephemeral sessions, we want to create a session command on one client
// and send it over to a second client that will process the command.
#[test]
fn should_send_and_receive_session_data() {
    // Create our client factory function that will be responsible for creating
    // all of our test clients.
    let basic_clients =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::new(basic_clients);

    // Create two clients, one will be used to create the session commands and
    // the other will used to receive the session commands.
    test_model
        .add_client(User::A)
        .expect("Should create a client");
    test_model
        .add_client(User::B)
        .expect("Should create a client");

    // Initialize the graph on client A
    let nonce = 1;
    test_model
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect("Should create a graph");

    // Sync the graph with client B. Currently, ephemeral commands must be run on
    // the same graph.
    test_model
        .sync(Graph::X, User::A, User::B)
        .expect("Should sync clients");

    // The session actions will create ephemeral commands that will only be
    // emitted within a ephemeral session and do not persist any changes
    // to the factDB. It can however persist changes to a temporary session
    // factDB that lives for the life of the session.

    // `session_actions` will create a session on client A, taking in our
    // actions and producing a series of serialized byte commands. These are our
    // ephemeral commands
    let (commands, _effects) = test_model
        .session_actions(
            User::A,
            Graph::X,
            [
                vm_action!(create_greeting("hello")),
                vm_action!(verify_hello()),
            ],
        )
        .expect("Should return effect");

    // Send commands to client B...

    // `session_receive` is used to receive and process the ephemeral commands
    // on a new client, in this case client B.
    let effects = test_model
        .session_receive(User::B, Graph::X, commands)
        .expect("should get effect");

    // Observe that our create_greeting action and our verification action
    // both succeeded.
    let expected = [
        vm_effect!(Greeting { msg: "hello" }),
        vm_effect!(Success { value: true }),
    ];
    assert_eq!(effects, expected);

    // Now we check the graphs and verify that our ephemeral command has not
    // been persisted to either of our client graphs.
    test_model
        .action(User::A, Graph::X, vm_action!(verify_hello()))
        .expect_err("should not persist fact to the graph");

    test_model
        .action(User::B, Graph::X, vm_action!(verify_hello()))
        .expect_err("should not persist fact to the graph");
}

// To test ephemeral sessions, we want to create a session command on one client
// and send it over to a second client that will process the command.
#[test]
fn should_send_and_receive_session_data_with_ffi_clients() {
    // Create our client factory, this will be responsible for creating all our clients.
    let ffi_clients = FfiClientFactory::new(FFI_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::new(ffi_clients);

    // Create two clients, one will be used to create the session commands and the
    // other will used to receive the session commands.
    test_model
        .add_client(User::A)
        .expect("Should create a client");
    test_model
        .add_client(User::B)
        .expect("Should create a client");

    // Retrieve the public keys for our client.
    let client_public_keys = test_model
        .get_public_keys(User::A)
        .expect("could not get public keys");

    // Pull off the public identity key.
    let client_ident_pk =
        postcard::to_allocvec(&client_public_keys.ident_pk).expect("should get ident pk");
    // Pull off the public signing key.
    let client_sign_pk =
        postcard::to_allocvec(&client_public_keys.sign_pk).expect("should get sign pk");

    // Initialize the graph on client A
    let nonce = 1;
    test_model
        .new_graph(
            Graph::X,
            User::A,
            vm_action!(init(nonce, client_sign_pk.clone())),
        )
        .expect("Should create a graph");

    // Add client keys to the fact db. Here we call the `add_user_keys` action
    // that takes in as arguments the public identity key and public signing key.
    test_model
        .action(
            User::A,
            Graph::X,
            vm_action!(add_user_keys(
                client_ident_pk.clone(),
                client_sign_pk.clone()
            )),
        )
        .expect("should add user");

    // Sync the graph with client B. Currently, ephemeral commands must be run on
    // the same graph.
    test_model
        .sync(Graph::X, User::A, User::B)
        .expect("Should sync clients");

    // The session actions will create ephemeral commands that will only be
    // emitted within a ephemeral session and do not persist any changes
    // to the factDB. It can however persist changes to a temporary session
    // factDB that lives for the life of the session.

    // `session_actions` will create a session on client A, taking in our
    // actions and producing a series of serialized byte commands. These are our
    // ephemeral commands
    let (commands, _effects) = test_model
        .session_actions(
            User::A,
            Graph::X,
            [
                vm_action!(create_greeting("hello")),
                vm_action!(verify_hello()),
            ],
        )
        .expect("Should return effect");

    // Send commands to client B...

    // `session_receive` is used to receive and process the ephemeral commands
    // on a new client, in this case client B.
    let effects = test_model
        .session_receive(User::B, Graph::X, commands)
        .expect("should get effect");

    // Observe that our create_greeting action and our verification action
    // both succeeded.
    let expected = [
        vm_effect!(Greeting { msg: "hello" }),
        vm_effect!(Success { value: true }),
    ];
    assert_eq!(effects, expected);

    // Now we check the graphs and verify that our ephemeral command has not
    // been persisted to either of our client graphs.
    test_model
        .action(User::A, Graph::X, vm_action!(verify_hello()))
        .expect_err("should not persist fact to the graph");

    test_model
        .action(User::B, Graph::X, vm_action!(verify_hello()))
        .expect_err("should not persist fact to the graph");
}

// We want to test that we can read the on-graph FactDB from a ephemeral
// command. To do this we will create a Fact, sync our graph and send a
// ephemeral command to a second client that will use that command to read from
// it's FactDB.
#[test]
fn should_allow_access_to_fact_db_from_session() {
    // Create our client factory, this will be responsible for creating all our clients.
    let basic_clients =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::new(basic_clients);

    // Create two clients, one will be used to create session commands and the
    // other will used to receive the session commands.
    test_model
        .add_client(User::A)
        .expect("Should create a client");
    test_model
        .add_client(User::B)
        .expect("Should create a client");

    let nonce = 1;
    // Initialize the graph on client A.
    test_model
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect("Should create a graph");

    // Create an on-graph fact to later be queried from the session command.
    let _ = test_model
        .action(User::A, Graph::X, vm_action!(create_action(42)))
        .expect("Should return effect");

    // Sync graph with client B. Currently, ephemeral commands must be run on
    // the same graph.
    test_model
        .sync(Graph::X, User::A, User::B)
        .expect("Should sync clients");

    // The first half of the model session API is used to create session commands.
    // Session actions will produce a collection of commands.
    let (commands, _effects) = test_model
        .session_actions(User::A, Graph::X, [vm_action!(get_stuff())])
        .expect("Should return effect");

    // Send commands to client B...

    // The second half of the model session API is to receive session commands
    // and process them on our second client.
    let effects = test_model
        .session_receive(User::B, Graph::X, commands)
        .expect("should get effect");

    // Observe that client B receives the commands from the client A session
    // and successfully processes the command to retrieve the current state of
    // the FactDB.
    assert_eq!(effects, [vm_effect!(StuffHappened { a: 1, x: 42 })])
}

// We want to test wether we can store our returned serialized ephemeral command
// data into the on-graph FactDB, because our returned session command is just
// serialized data, that is completely possible.
#[test]
fn should_store_session_data_to_graph() {
    // Create our client factory, this will be responsible for creating all our clients.
    let basic_clients =
        BasicClientFactory::new(BASIC_POLICY).expect("should create client factory");
    // Create a new model with our client factory.
    let mut test_model = RuntimeModel::new(basic_clients);

    // Create client, this client will be used to create the session commands.
    test_model
        .add_client(User::A)
        .expect("Should create a client");

    let nonce = 1;
    // Initialize the graph on client A.
    test_model
        .new_graph(Graph::X, User::A, vm_action!(init(nonce)))
        .expect("Should create a graph");

    // `sessions_actions` is the portion of the session api responsible for
    // creating session commands.
    let (commands, _effects) = test_model
        .session_actions(User::A, Graph::X, [vm_action!(create_greeting("hello"))])
        .expect("Should return effect");

    // We want to test that we can take the serialized byte command from the
    // session and store it into an on-graph fact value.

    // Pull off the first command from the collection.
    let session_cmd = commands
        .first()
        .expect("should get value")
        .clone()
        .into_vec();

    // Because these commands are just data, we can store it just like any other
    // byte data, using it as the argument of an on-graph action, like
    // `store_session_data`.
    let effects = test_model
        .action(
            User::A,
            Graph::X,
            vm_action!(store_session_data("say_hello", session_cmd)),
        )
        .expect("Should return effect");

    // Observe that it is successfully added to the graph.
    assert_eq!(effects, [vm_effect!(Success { value: true })]);
}

#[test]
fn can_perform_action_after_receive_on_session() -> anyhow::Result<()> {
    let basic_clients = BasicClientFactory::new(BASIC_POLICY)?;
    let mut test_model = RuntimeModel::new(basic_clients);

    // Create clients
    test_model.add_client(User::A)?;
    test_model.add_client(User::B)?;

    // Create graph and sync
    test_model.new_graph(Graph::X, User::A, vm_action!(init(42)))?;
    test_model.sync(Graph::X, User::A, User::B)?;

    // Perform actions on client A session.
    let (cmds, effects) = test_model.session_actions(
        User::A,
        Graph::X,
        [vm_action!(create_action(5)), vm_action!(increment(3))],
    )?;

    assert_eq!(
        effects,
        [
            vm_effect!(StuffHappened { a: 1, x: 5 }),
            vm_effect!(StuffHappened { a: 1, x: 8 }),
        ]
    );

    // Receive commands and perform action on client B session.
    let mut session = test_model.session(User::B, Graph::X)?;
    for cmd in cmds {
        session.receive(&cmd)?;
    }
    session.action(vm_action!(increment(7)))?;

    let (cmds, effects) = session.observe();
    assert_eq!(
        effects,
        [
            vm_effect!(StuffHappened { a: 1, x: 5 }),
            vm_effect!(StuffHappened { a: 1, x: 8 }),
            vm_effect!(StuffHappened { a: 1, x: 15 }),
        ]
    );

    // Receive commands from client B on a new session,
    // and then perform an action afterward.
    let mut session = test_model.session(User::A, Graph::X)?;
    session.action(vm_action!(create_action(2)))?;
    for cmd in cmds {
        session.receive(&cmd)?;
    }

    let (_cmds, effects) = session.observe();
    assert_eq!(
        effects,
        [
            vm_effect!(StuffHappened { a: 1, x: 2 }),
            vm_effect!(StuffHappened { a: 1, x: 9 }),
        ]
    );

    Ok(())
}

// We want to test that we can create clients that use different key bundles, can
// be synced, and can issue and receive ephemeral commands.
#[test]
fn should_create_clients_with_args() {
    // Create our client factory, this will be responsible for creating all our
    // clients.
    let client_factory = IdentityClientFactory(PhantomData);
    // Create a new model instance with our client factory.
    let mut test_model = RuntimeModel::new(client_factory);

    let ffi_schema: &[ModuleSchema<'static>] = &[
        DeviceFfi::SCHEMA,
        EnvelopeFfi::SCHEMA,
        PerspectiveFfi::SCHEMA,
        CryptoFfi::<Store>::SCHEMA,
        IdamFfi::<Store>::SCHEMA,
    ];

    let policy_ast = parse_policy_document(FFI_POLICY).unwrap();
    // Create policy machine
    let module = Compiler::new(&policy_ast)
        .ffi_modules(ffi_schema)
        .compile()
        .unwrap();
    let machine = Machine::from_module(module).expect("should be able to load compiled module");

    // We'll store the pub keys necessary for initializing and interacting with
    // the graph.
    let public_keys;

    // Create first client with full key bundle (user_id and sign_id)
    test_model
        .add_client_with(User::A, {
            // Setup keystore
            let temp_dir = tempdir().expect("should create temp directory");
            let root = temp_dir.into_path().join("client");
            assert!(
                !root.try_exists().expect("should create root path"),
                "duplicate client name"
            );
            let mut store = {
                let path = root.join("keystore");
                fs::create_dir_all(&path).expect("should create directory");
                Store::open(&path).expect("should create keystore")
            };

            let (mut eng, _) = DefaultEngine::from_entropy(Rng);
            // Generate key bundle
            let bundle =
                KeyBundle::generate(&mut eng, &mut store).expect("unable to generate `KeyBundle`");

            // Assign public keys to our variable
            public_keys = bundle
                .public_keys(&mut eng, &store)
                .expect("unable to generate public keys");

            // Configure FFIs
            let ffis: Vec<Box<dyn FfiCallable<DefaultEngine> + Send + 'static>> = vec![
                Box::from(DeviceFfi::new(bundle.user_id)),
                Box::from(EnvelopeFfi),
                Box::from(PerspectiveFfi),
                Box::from(CryptoFfi::new(
                    store.try_clone().expect("should clone key store"),
                )),
                Box::from(IdamFfi::new(store)),
            ];

            let policy = VmPolicy::new(machine.clone(), eng, ffis).expect("should create policy");
            let engine = ModelEngine::new(policy);
            let provider = MemStorageProvider::new();

            ModelClient {
                state: RefCell::new(ClientState::new(engine, provider)),
                public_keys: EmptyKeys,
            }
        })
        .expect("Should create a client");

    // Pull off the public signing and identity key.
    let client_sign_pk = postcard::to_allocvec(&public_keys.sign_pk).expect("should get sign pk");
    let client_ident_pk =
        postcard::to_allocvec(&public_keys.ident_pk).expect("should get ident pk");

    let nonce = 1;
    // Create a graph for client A. The init command in the ffi policy
    // required the public signing key.
    test_model
        .new_graph(
            Graph::X,
            User::A,
            vm_action!(init(nonce, client_sign_pk.clone())),
        )
        .expect("Should create a graph");

    // Add client keys to the fact db. Here we call the `add_user_keys` action
    // that takes in as arguments the public identity key and public signing key.
    test_model
        .action(
            User::A,
            Graph::X,
            vm_action!(add_user_keys(
                client_ident_pk.clone(),
                client_sign_pk.clone()
            )),
        )
        .expect("should add user");

    // Create second client with minimal key bundle (only user_id)
    test_model
        .add_client_with(User::B, {
            // Setup keystore
            let temp_dir = tempdir().expect("should create temp directory");
            let root = temp_dir.into_path().join("client");
            assert!(
                !root.try_exists().expect("should create root path"),
                "duplicate client name"
            );
            let mut store = {
                let path = root.join("keystore");
                fs::create_dir_all(&path).expect("should create directory");
                Store::open(&path).expect("should create keystore")
            };

            let (mut eng, _) = DefaultEngine::from_entropy(Rng);
            // Generate key bundle
            let bundle = MinKeyBundle::generate(&mut eng, &mut store)
                .expect("unable to generate `KeyBundle`");

            // Configure FFIs
            let ffis: Vec<Box<dyn FfiCallable<DefaultEngine> + Send + 'static>> = vec![
                Box::from(DeviceFfi::new(bundle.user_id)),
                Box::from(EnvelopeFfi),
                Box::from(PerspectiveFfi),
                Box::from(CryptoFfi::new(
                    store.try_clone().expect("should clone key store"),
                )),
                Box::from(IdamFfi::new(store)),
            ];

            let policy = VmPolicy::new(machine.clone(), eng, ffis).expect("should create policy");
            let engine = ModelEngine::new(policy);
            let provider = MemStorageProvider::new();

            ModelClient {
                state: RefCell::new(ClientState::new(engine, provider)),
                public_keys: EmptyKeys,
            }
        })
        .expect("Should create a client");

    // Sync client B with client A (A -> B). Syncs are unidirectional, client
    // B will receive all the new commands it doesn't yet know about from client
    // A. At this stage of the test, that's the init, add_user_keys.
    test_model
        .sync(Graph::X, User::A, User::B)
        .expect("Should sync clients");

    let (commands, _effects) = test_model
        .session_actions(
            User::A,
            Graph::X,
            [
                vm_action!(create_greeting("hello")),
                vm_action!(verify_hello()),
            ],
        )
        .expect("Should return effect");

    // Send commands to client B...

    // `session_receive` is used to receive and process the ephemeral commands
    // on a new client, in this case client B.
    let effects = test_model
        .session_receive(User::B, Graph::X, commands)
        .expect("should get effect");

    // Observe that our create_greeting action and our verification action
    // both succeeded.
    let expected = [
        vm_effect!(Greeting { msg: "hello" }),
        vm_effect!(Success { value: true }),
    ];
    assert_eq!(effects, expected);
}
