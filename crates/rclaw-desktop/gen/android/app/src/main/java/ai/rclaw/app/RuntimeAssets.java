package ai.rclaw.app;

import android.content.Context;
import android.content.pm.PackageManager;
import android.content.res.AssetManager;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.Arrays;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Set;

final class RuntimeAssets {
    private static final String SKILL_ASSET_ROOT = "rclaw-skills";
    static final int MAX_SKILL_CONFIG_BYTES = 1024 * 1024;

    private RuntimeAssets() {}

    static void installBundledSkills(Context context) throws IOException {
        File destination = new File(context.getFilesDir(), "runtime/bundled-skills");
        File marker = new File(context.getFilesDir(), "runtime/bundled-skills.version");
        File privateRoot = context.getFilesDir().getAbsoluteFile();
        if (Files.isSymbolicLink(destination.toPath())) {
            deleteRecursively(destination, privateRoot);
        }
        if (Files.isSymbolicLink(marker.toPath())) {
            deleteRecursively(marker, privateRoot);
        }
        ensureNoSymlinkAncestors(destination, privateRoot);
        ensureNoSymlinkAncestors(marker, privateRoot);
        String version = packageVersion(context);
        if (destination.isDirectory() && marker.isFile()) {
            String installed = new String(
                    Files.readAllBytes(marker.toPath()),
                    StandardCharsets.UTF_8).trim();
            if (version.equals(installed)) {
                return;
            }
        }
        copyAssetTree(context.getAssets(), SKILL_ASSET_ROOT, destination, privateRoot);
        writeMarker(marker, version, privateRoot);
    }

    static Map<String, String> loadBundledSkillConfigs(Context context) throws IOException {
        Map<String, String> configs = new LinkedHashMap<>();
        collectSkillConfigs(
                context.getAssets(),
                SKILL_ASSET_ROOT,
                "",
                configs);
        return configs;
    }

    private static void collectSkillConfigs(
            AssetManager assets,
            String assetPath,
            String relativePath,
            Map<String, String> configs) throws IOException {
        String[] children = assets.list(assetPath);
        if (children != null && children.length > 0) {
            Arrays.sort(children);
            for (String child : children) {
                collectSkillConfigs(
                        assets,
                        assetPath + "/" + child,
                        relativePath.isEmpty() ? child : relativePath + "/" + child,
                        configs);
            }
            return;
        }
        if (!isSkillConfigName(new File(relativePath).getName())) {
            return;
        }
        try (InputStream input = assets.open(assetPath)) {
            configs.put(relativePath, readSkillConfig(input));
        }
    }

    private static boolean isSkillConfigName(String name) {
        return name.equals(".env")
                || name.equals("config.env")
                || name.equals("config.toml")
                || name.equals("config.json");
    }

    private static String readSkillConfig(InputStream input) throws IOException {
        ByteArrayOutputStream output = new ByteArrayOutputStream();
        byte[] buffer = new byte[8192];
        int total = 0;
        int read;
        while ((read = input.read(buffer)) != -1) {
            total += read;
            if (total > MAX_SKILL_CONFIG_BYTES) {
                throw new IOException("skill 配置文件超过 1 MiB");
            }
            output.write(buffer, 0, read);
        }
        return output.toString(StandardCharsets.UTF_8.name());
    }

    private static String packageVersion(Context context) throws IOException {
        try {
            return Long.toString(context.getPackageManager()
                    .getPackageInfo(context.getPackageName(), 0)
                    .lastUpdateTime);
        } catch (PackageManager.NameNotFoundException error) {
            throw new IOException("无法读取 APK 更新时间", error);
        }
    }

    private static void writeMarker(
            File marker,
            String version,
            File privateRoot) throws IOException {
        File parent = marker.getParentFile();
        if (parent == null || (!parent.isDirectory() && !parent.mkdirs())) {
            throw new IOException("无法创建运行时版本目录: " + marker);
        }
        File temporary = new File(parent, marker.getName() + ".tmp");
        if (Files.isSymbolicLink(temporary.toPath())) {
            deleteRecursively(temporary, privateRoot);
        }
        ensureNoSymlinkAncestors(temporary, privateRoot);
        try (FileOutputStream output = new FileOutputStream(temporary, false)) {
            output.write(version.getBytes(StandardCharsets.UTF_8));
            output.getFD().sync();
        }
        if (marker.exists() && !marker.delete()) {
            throw new IOException("无法更新运行时资源版本: " + marker);
        }
        if (!temporary.renameTo(marker)) {
            throw new IOException("无法提交运行时资源版本: " + marker);
        }
    }

    private static void copyAssetTree(
            AssetManager assets,
            String assetPath,
            File destination,
            File privateRoot) throws IOException {
        if (Files.isSymbolicLink(destination.toPath())) {
            deleteRecursively(destination, privateRoot);
        }
        ensureNoSymlinkAncestors(destination, privateRoot);
        String[] children = assets.list(assetPath);
        if (children != null && children.length > 0) {
            if (destination.exists() && !destination.isDirectory()) {
                deleteRecursively(destination, privateRoot);
            }
            if (!destination.isDirectory() && !destination.mkdirs()) {
                throw new IOException("无法创建运行时资源目录: " + destination);
            }
            Set<String> expected = new HashSet<>(Arrays.asList(children));
            for (String child : children) {
                copyAssetTree(
                        assets,
                        assetPath + "/" + child,
                        new File(destination, child),
                        privateRoot);
            }
            File[] installed = destination.listFiles();
            if (installed == null) {
                throw new IOException("无法读取运行时资源目录: " + destination);
            }
            for (File child : installed) {
                if (!expected.contains(child.getName())) {
                    deleteRecursively(child, privateRoot);
                }
            }
            return;
        }

        if (destination.isDirectory()) {
            deleteRecursively(destination, privateRoot);
        }
        File parent = destination.getParentFile();
        if (parent == null || (!parent.isDirectory() && !parent.mkdirs())) {
            throw new IOException("无法创建运行时资源父目录: " + destination);
        }
        File temporary = new File(parent, destination.getName() + ".tmp");
        if (Files.isSymbolicLink(temporary.toPath())) {
            deleteRecursively(temporary, privateRoot);
        }
        ensureNoSymlinkAncestors(temporary, privateRoot);
        try (InputStream input = assets.open(assetPath);
                FileOutputStream output = new FileOutputStream(temporary, false)) {
            byte[] buffer = new byte[16 * 1024];
            int read;
            while ((read = input.read(buffer)) != -1) {
                output.write(buffer, 0, read);
            }
            output.getFD().sync();
        }
        if (destination.exists() && !destination.delete()) {
            throw new IOException("无法更新运行时资源: " + destination);
        }
        if (!temporary.renameTo(destination)) {
            throw new IOException("无法提交运行时资源: " + destination);
        }
    }

    private static void deleteRecursively(File target, File privateRoot) throws IOException {
        File absoluteTarget = target.getAbsoluteFile();
        ensureLexicallyInside(absoluteTarget, privateRoot);
        if (Files.isSymbolicLink(absoluteTarget.toPath())) {
            Files.deleteIfExists(absoluteTarget.toPath());
            return;
        }
        File canonicalTarget = target.getCanonicalFile();
        String rootPath = privateRoot.getCanonicalPath();
        String targetPath = canonicalTarget.getPath();
        if (!targetPath.startsWith(rootPath + File.separator)) {
            throw new IOException("拒绝删除 app 私有目录之外的资源: " + target);
        }
        if (canonicalTarget.isDirectory()) {
            // Keep the lexical /data/user/0 form for recursive checks. Listing
            // through canonicalTarget would switch children to the equivalent
            // /data/data alias and make the next lexical boundary check fail.
            File[] children = absoluteTarget.listFiles();
            if (children == null) {
                throw new IOException("无法读取待清理资源目录: " + absoluteTarget);
            }
            for (File child : children) {
                deleteRecursively(child, privateRoot);
            }
        }
        if (canonicalTarget.exists() && !canonicalTarget.delete()) {
            throw new IOException("无法清理旧运行时资源: " + canonicalTarget);
        }
    }

    private static void ensureNoSymlinkAncestors(File target, File privateRoot)
            throws IOException {
        File absoluteTarget = target.getAbsoluteFile();
        ensureLexicallyInside(absoluteTarget, privateRoot);
        File current = absoluteTarget;
        while (current != null && !current.equals(privateRoot)) {
            if (Files.isSymbolicLink(current.toPath())) {
                throw new IOException("运行时资源路径不允许包含符号链接: " + current);
            }
            current = current.getParentFile();
        }
        String canonical = absoluteTarget.getCanonicalPath();
        String rootPath = privateRoot.getCanonicalPath();
        if (!canonical.startsWith(rootPath + File.separator)) {
            throw new IOException("运行时资源路径超出 app 私有目录: " + absoluteTarget);
        }
    }

    private static void ensureLexicallyInside(File target, File privateRoot) throws IOException {
        String rootPath = privateRoot.getAbsoluteFile().toPath().normalize().toString();
        String targetPath = target.toPath().normalize().toString();
        if (!targetPath.startsWith(rootPath + File.separator)) {
            throw new IOException("运行时资源路径超出 app 私有目录: " + target);
        }
    }
}
