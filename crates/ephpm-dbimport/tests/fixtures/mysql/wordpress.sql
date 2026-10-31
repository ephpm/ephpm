-- MySQL dump 10.13  Distrib 8.0.36, for Linux (x86_64)
--
-- Host: localhost    Database: blog
-- ------------------------------------------------------
-- Server version	8.0.36

/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
/*!40101 SET NAMES utf8mb4 */;
/*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */;
/*!40103 SET TIME_ZONE='+00:00' */;

--
-- Table structure for table `wp_posts`
--

DROP TABLE IF EXISTS `wp_posts`;
CREATE TABLE `wp_posts` (
  `ID` bigint unsigned NOT NULL AUTO_INCREMENT,
  `post_title` text COLLATE utf8mb4_unicode_ci NOT NULL,
  `post_status` enum('publish','draft','trash') COLLATE utf8mb4_unicode_ci NOT NULL DEFAULT 'draft',
  `comment_status` set('open','closed') COLLATE utf8mb4_unicode_ci NOT NULL DEFAULT 'open',
  `post_content` longtext COLLATE utf8mb4_unicode_ci NOT NULL,
  PRIMARY KEY (`ID`),
  KEY `type_status_date` (`post_status`),
  FULLTEXT KEY `post_content_ft` (`post_content`)
) ENGINE=InnoDB AUTO_INCREMENT=42 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci ROW_FORMAT=DYNAMIC;

--
-- Dumping data for table `wp_posts`
--

LOCK TABLES `wp_posts` WRITE;
/*!40000 ALTER TABLE `wp_posts` DISABLE KEYS */;
INSERT INTO `wp_posts` VALUES (1,'Hello World','publish','open','Welcome to the blog; it\'s great.'),(2,'Draft Idea','draft','closed','Not; ready yet.'),(3,'Trashed','trash','open','Gone.');
/*!40000 ALTER TABLE `wp_posts` ENABLE KEYS */;
UNLOCK TABLES;

--
-- A trigger (mysqldump's gate-split spelling)
--

/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER `wp_posts_bi` BEFORE INSERT ON `wp_posts` FOR EACH ROW BEGIN SET NEW.post_title = TRIM(NEW.post_title); END */;

--
-- A stored procedure (DELIMITER-wrapped, as mysqldump emits them)
--

DELIMITER ;;
CREATE DEFINER=`root`@`localhost` PROCEDURE `purge_trash`()
BEGIN
  DELETE FROM wp_posts WHERE post_status = 'trash';
END ;;
DELIMITER ;

/*!40103 SET TIME_ZONE=@OLD_TIME_ZONE */;
/*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */;
-- Dump completed
