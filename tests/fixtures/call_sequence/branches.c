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
