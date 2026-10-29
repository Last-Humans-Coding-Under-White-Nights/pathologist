namespace OHOS { namespace DataShare {
class Backend {
public:
    int OpenFile(int x) { return x; }
};
class DataShareStub : public Backend {};
class Provider : public DataShareStub {};
class OtherProvider : public DataShareStub {};
class DataShareHelper {
public:
    int OpenFile(int x);
};
int use(DataShareHelper *h) { return h->OpenFile(1); }
}}
