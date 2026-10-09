/* Small pointer-flow example for pathologist issue #140. */
struct Operations {
    void (*send)(char *payload);
};

struct Connection {
    struct Operations *ops;
};

struct Client {
    struct Connection *connection;
};

static char *saved_payload;

static void save_payload(char *payload) {
    saved_payload = payload;
}

static struct Operations operations = { .send = save_payload };
static struct Connection connection = { .ops = &operations };
static struct Client client = { .connection = &connection };

static char *identity(char *value) {
    return value;
}

static void dispatch(struct Client *receiver, char *payload) {
    char *forwarded = identity(payload);
    receiver->connection->ops->send(forwarded);
}

int main(void) {
    char message[] = "hello";
    char *payload = message;
    struct Client *receiver = &client;
    dispatch(receiver, payload);
    extern int run_cpp_cases(void);
    if (run_cpp_cases() != 0) return 2;
    extern int run_macro_cases(void);
    if (run_macro_cases() != 0) return 3;
    return saved_payload == payload ? 0 : 1;
}
