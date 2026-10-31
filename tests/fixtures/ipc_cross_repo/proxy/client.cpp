namespace OHOS { namespace DataShare {
class IDataShare {
public:
    virtual int OpenFile(int uri) = 0;
};
class DataShareProxy : public IRemoteProxy<IDataShare> {
public:
    int OpenFile(int uri) { return OpenFileInner(uri); }
    int OpenFileInner(int uri) { remote->SendRequest(uri); return uri; }
};
class DataShareHelper {
public:
    virtual int OpenFile(int uri) = 0;
};
class DataShareHelperImpl : public DataShareHelper {
    DataShareProxy *proxy;
public:
    int OpenFile(int uri) override { return proxy->OpenFile(uri); }
};
}}
