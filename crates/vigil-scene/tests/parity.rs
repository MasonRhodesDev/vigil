use vigil_scene::Scene;

#[test]
fn geometry_matches_desktop_boot_scene_py() {
    let table = include_str!("fixtures/scene-py-geometry.tsv");
    let mut lines = table.lines();
    let header: Vec<&str> = lines.next().unwrap().split('\t').collect();
    let mut rows = 0;
    for line in lines {
        let v: Vec<i64> = line.split('\t').map(|f| f.parse().unwrap()).collect();
        let col = |k: &str| v[header.iter().position(|h| *h == k).unwrap()];
        let spec = Scene {
            ref_short_side: (col("ref") > 0).then(|| col("ref") as u32),
            ..Scene::default()
        };
        let g = spec.geometry(col("w") as u32, col("h") as u32);
        let got = [
            i64::from(g.logo),
            i64::from(g.glow),
            g.logo_x,
            g.logo_y,
            g.glow_x,
            g.glow_y,
            i64::from(g.bar_w),
            i64::from(g.bar_h),
            g.bar_x,
            g.bar_y,
            i64::from(g.gap),
        ];
        assert_eq!(got.as_slice(), &v[3..], "{line}");
        rows += 1;
    }
    assert!(rows > 200, "{rows} rows");
}
