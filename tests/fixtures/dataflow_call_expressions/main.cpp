static void consume(char *value) {}
static char *identity(char *value) { return value; }
struct Base { virtual void send(char *value) {} };
struct Derived : Base { void send(char *value) override {} };
#define CONSUME(v) consume(v)
#define TWICE(v) consume(v); consume((v))
void run(char *payload, void (*callback)(char *), Base *receiver) {
    consume(payload);
    callback(payload);
    CONSUME(payload);
    TWICE(payload);
    char *forwarded = identity(payload);
    consume(forwarded);
    receiver->send(payload);
    consume(identity(payload));
    consume(
        payload
    );
}
void launch(char *payload) {
    Derived receiver;
    run(payload, consume, &receiver);
}
