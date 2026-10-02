/* Namespace-static calls with distinct third-argument values. */
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

namespace epta {
    static void dispatch(struct Client *receiver, char *payload, char *payload1) {
        char *forwarded = identity(payload);
        receiver->connection->ops->send(forwarded);
    }
};

int main(void) {
    char message[] = "hello";
    char *payload = message;
    char *payload1 = payload;

    struct Client *receiver = &client;
    epta::dispatch(receiver, payload, payload1);
    epta::dispatch(receiver, payload, payload);
    epta::dispatch(receiver, payload, nullptr);
    return saved_payload == payload ? 0 : 1;
}
