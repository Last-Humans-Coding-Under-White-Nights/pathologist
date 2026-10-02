struct Client { void (*send)(char *); };
static void save(char *value) {}
static Client client = { .send = save };
namespace epta {
static void dispatch(Client *receiver, char *payload, char *payload1) {
    receiver->send(payload);
}
}
static void only_null(void (*callback)(char *)) { callback(nullptr); }
int main() {
    char message[] = "hello";
    char *payload = message;
    char *payload1 = payload;
    Client *receiver = &client;
    epta::dispatch(receiver, payload, payload1);
    epta::dispatch(receiver, payload, nullptr);
    only_null(nullptr);
}
