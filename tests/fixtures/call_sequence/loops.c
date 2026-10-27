void init(void);
int more(void);
void step(void);
void body(void);
int again(void);
int stop_now(void);
void skip(void);
void after(void);

void for_loop(void)
{
    for (init(); more(); step()) {
        body();
        if (again())
            continue;
        if (stop_now())
            break;
        skip();
    }
    after();
}

void do_loop(void)
{
    do {
        body();
    } while (more());
    after();
}

void while_loop(void)
{
    while (more())
        body();
    after();
}

void first(void);
void second(void);

void loop_pair(void)
{
    while (more()) {
        first();
        second();
    }
}
