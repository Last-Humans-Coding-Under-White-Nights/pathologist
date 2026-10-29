void prepare(void);
void send(void);
void recover(void);
void finish(void);

void run(int ok)
{
    prepare();
    if (ok)
        send();
    else
        recover();
    finish();
}

int selector(void);
int chosen(void);
int other(void);
void picked(void);

void generic_pick(void)
{
    _Generic(selector(), int: chosen(), default: other());
    picked();
}
