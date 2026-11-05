package @APPLICATION_ID@;

/** Loads the JNI library without failing activity class initialization. */
final class NativeLibraryLoader {
    private NativeLibraryLoader() { }

    /** Returns null on success or a readable load error that the bootstrap activity can display. */
    static String load(String library) {
        try {
            System.loadLibrary(library);
            return null;
        } catch (LinkageError | SecurityException error) {
            return error.getClass().getSimpleName() + ": " + String.valueOf(error.getMessage());
        }
    }
}
