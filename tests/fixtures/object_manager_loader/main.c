/* Issue #127 review (P1): HDF's driver-loader singleton. An object is built
 * by a creator reached through a table, handed out as its base type, cached
 * in a pointer static, and its methods are called through the interface. */
#include <stddef.h>

struct HdfObject { int objectId; };
struct HdfDriver;

struct IDriverLoader {
    struct HdfObject object;
    struct HdfDriver *(*GetDriver)(const char *driverName);
    void (*ReclaimDriver)(struct HdfDriver *driver);
};

struct HdfDriverLoader {
    struct IDriverLoader super;
};

struct HdfObjectCreator {
    struct HdfObject *(*Create)(void);
    void (*Release)(struct HdfObject *);
};

struct HdfDriver *HdfDriverLoaderGetDriver(const char *driverName) { (void)driverName; return NULL; }
void HdfDriverLoaderReclaimDriver(struct HdfDriver *driver) { (void)driver; }

void HdfDriverLoaderConstruct(struct HdfDriverLoader *inst)
{
    if (inst != NULL) {
        inst->super.GetDriver = HdfDriverLoaderGetDriver;
        inst->super.ReclaimDriver = HdfDriverLoaderReclaimDriver;
    }
}

struct HdfObject *HdfDriverLoaderCreate(void)
{
    static int isDriverLoaderInit = 0;
    static struct HdfDriverLoader driverLoader;
    if (!isDriverLoaderInit) {
        HdfDriverLoaderConstruct(&driverLoader);
        isDriverLoaderInit = 1;
    }
    return (struct HdfObject *)&driverLoader;
}

static const struct HdfObjectCreator g_objectCreators[] = {
    [0] = { .Create = HdfDriverLoaderCreate, .Release = NULL },
};

const struct HdfObjectCreator *HdfObjectManagerGetCreators(int objectId)
{
    return &g_objectCreators[objectId];
}

struct HdfObject *HdfObjectManagerGetObject(int objectId)
{
    struct HdfObject *object = NULL;
    const struct HdfObjectCreator *targetCreator = HdfObjectManagerGetCreators(objectId);
    if ((targetCreator != NULL) && (targetCreator->Create != NULL)) {
        object = targetCreator->Create();
        if (object != NULL) {
            object->objectId = objectId;
        }
    }
    return object;
}

struct IDriverLoader *HdfDriverLoaderGetInstance(void)
{
    static struct IDriverLoader *instance = NULL;
    if (instance == NULL) {
        instance = (struct IDriverLoader *)HdfObjectManagerGetObject(0);
    }
    return instance;
}

int DevHostServiceAddDevice(const char *moduleName)
{
    struct IDriverLoader *driverLoader = HdfDriverLoaderGetInstance();
    struct HdfDriver *driver = driverLoader->GetDriver(moduleName);
    if (driver != NULL) {
        driverLoader->ReclaimDriver(driver);
    }
    return 0;
}
